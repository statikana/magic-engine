use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use mtg_ir::{
    AbilityDef, ActivatedAbility, ActivationRestriction, ActivatorWho, Affected, Amount, CardDef,
    CardLayout, CardTypeMask, Characteristics, ColorMask, CompareOp, Condition,
    ConditionalCostReduction, CostDef, CostDelta, CostDeltaMode, CostObjectTarget, CounterTarget,
    EffectOp, EffectStep, EtbChoiceSpec, EventDetail, EventFilter, EventKind, EventPattern,
    FaceDef, FilterFlags, GamePhase, KeywordAbility, KeywordDef, Layer, LayerMod, ManaColor,
    ManaCost, ManaProduction, ModalDef, ObjectFilter, ObjectPredicate, OptionalMode, OptionalSpec,
    PlayerFilter, PlayerRef, PlayerRelation, PrintedPt, Ref, ReflexiveEventFilter,
    ReflexiveTriggerSpec, RelativeControl, ReplacementAbility, ReplacementDef, ReplacementWith,
    RestrictionKind, Selector, Span, SpellAbility, SpellCastRestrictionKind, SpellOrAbilityFilter,
    StatKind, StaticAbility, StaticBody, StaticDef, StepGate, Timing, TriggerDef, TriggerScope,
    TriggerSubject, TriggeredAbility, UnsupportedAbility, UnsupportedReason, Zone, ZoneChangeMod,
    ZoneMask,
};
use smallvec::{SmallVec, smallvec};

use crate::error::CompileError;
use crate::input::{
    RawCardInput, RawFaceInput, build_characteristics, effective_layout, get_card_faces,
    is_character_legend,
};
use crate::intern::Interner;
use crate::lexer::{Punct, TokenKind, tokenize};
use crate::normalize::{NormalizedFace, NormalizedLine, normalize_face, split_sentences};
use crate::parser::cost::{
    CostParse, is_cast_payable_cost, is_supported_optional_cost, lookup_counter_id,
    parse_additional_cost_sentence, parse_cost_list, parse_cost_modifier_sentence,
    parse_loyalty_cost,
};
use crate::parser::effect::{
    EffectParse, ModalBullet, extract_unparsed_targets, interpretable_trigger, mana_step_complete,
    parse_effects_with_quoted, parse_effects_with_quoted_and_cond,
    parse_expanded_modal_with_quoted, parse_modal_with_quoted, parse_spell_face,
    statically_interpretable,
};
use crate::parser::filter::{FilterOpts, NullScope, parse_multi_or_object_phrase};
use crate::parser::keyword::{
    KeywordImplKind, get_keyword_spec, parse_keyword_line, starts_with_ci,
};
use crate::parser::static_def::{
    StaticItem, parse_spell_alt_cast_or_flash_sentence, parse_static_spell_restriction_or_cast_mod,
    parse_statics,
};
use crate::parser::trigger::{
    ActivatedExtraRestriction, parse_activated_if_condition_sentence, parse_p12_trigger_header,
    parse_trigger_header, runtime_trigger_problem,
};
use crate::parser::value::{parse_condition, parse_small_number};
use crate::parser::{AbilityContext, ParseCtx, QuotedAbilityCompiler, Wave4RewriteOpts};

const SUPPORTED_MULTIFACE_LAYOUTS: [CardLayout; 8] = [
    CardLayout::Split,
    CardLayout::ModalDfc,
    CardLayout::Adventure,
    CardLayout::Transform,
    CardLayout::Flip,
    CardLayout::Meld,
    CardLayout::Prepare,
    CardLayout::Omen,
];

fn raw_card_as_face(card: &RawCardInput) -> RawFaceInput {
    RawFaceInput {
        name: card.name.clone(),
        mana_cost: card.mana_cost.clone(),
        type_line: card.type_line.clone(),
        oracle_text: card.oracle_text.clone(),
        power: card.power.clone(),
        toughness: card.toughness.clone(),
        loyalty: card.loyalty.clone(),
        defense: card.defense.clone(),
        colors: card.colors.clone(),
        color_indicator: card.color_indicator.clone(),
        keywords: Vec::new(),
        cmc: None,
    }
}

pub(crate) fn unsupported_ability(
    reason: UnsupportedReason,
    text: String,
    span: Span,
    unparsed: Vec<Span>,
) -> AbilityDef {
    AbilityDef::Unsupported(UnsupportedAbility {
        reason,
        text,
        span,
        unparsed,
        id: None,
        cost: Vec::new(),
        cost_complete: None,
        cost_keyword: None,
        timing: None,
        is_mana: false,
        effect: Vec::new(),
        targets: Vec::new(),
        activation_restrictions: Vec::new(),
        activation_condition: None,
        activator_who: None,
        power_up_reduction: false,
        x_cant_be_zero: false,
        activation_cost_reduction: None,
        cast_restrictions: Vec::new(),
        cast_condition: None,
        trigger: None,
        condition: None,
        grant_condition: None,
    })
}

#[derive(Default)]
struct CardQuotedAbilityCompiler {
    cache: RefCell<HashMap<(String, String), Vec<AbilityDef>>>,
    depth: Cell<u8>,
}

impl QuotedAbilityCompiler for CardQuotedAbilityCompiler {
    fn compile_quoted_ability(
        &self,
        ability_text: &str,
        card_name: &str,
        interner: &mut Interner,
    ) -> Vec<AbilityDef> {
        let trimmed = ability_text.trim();
        if trimmed.is_empty() || self.depth.get() >= 2 {
            return Vec::new();
        }
        let key = (trimmed.to_string(), card_name.to_string());
        if let Some(cached) = self.cache.borrow().get(&key) {
            return cached.clone();
        }
        let raw_face = RawFaceInput {
            name: card_name.to_string(),
            mana_cost: String::new(),
            type_line: "Creature".to_string(),
            oracle_text: trimmed.to_string(),
            power: None,
            toughness: None,
            loyalty: None,
            defense: None,
            colors: Vec::new(),
            color_indicator: None,
            keywords: Vec::new(),
            cmc: None,
        };
        self.depth.set(self.depth.get() + 1);
        let compiled =
            compile_single_face_with_quoted(&raw_face, &[], CardLayout::Normal, interner, self);
        self.depth.set(self.depth.get() - 1);
        let abilities = compiled.map(|f| f.abilities).unwrap_or_default();
        self.cache.borrow_mut().insert(key, abilities.clone());
        abilities
    }
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn find_word_ci_from(hay: &str, needle: &str, from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < from + needle.len() {
        return None;
    }
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    let b0_lo = n[0].to_ascii_lowercase();
    let b0_hi = n[0].to_ascii_uppercase();
    let limit = h.len() - n.len();
    let mut i = from;
    while i <= limit {
        let b = h[i];
        if (b == b0_lo || b == b0_hi) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            let left_ok = i == 0 || !is_word_byte(h[i - 1]);
            let right_ok = i + n.len() == h.len() || !is_word_byte(h[i + n.len()]);
            if left_ok && right_ok && hay.is_char_boundary(i) && hay.is_char_boundary(i + n.len()) {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[inline]
fn contains_word_ci(hay: &str, needle: &str) -> bool {
    find_word_ci_from(hay, needle, 0).is_some()
}

fn replace_word_ci(input: &str, from: &str, to: &str) -> String {
    let mut pos = 0usize;
    let Some(first) = find_word_ci_from(input, from, pos) else {
        return input.to_string();
    };
    let mut out = String::with_capacity(input.len());
    let mut cur_match = first;
    loop {
        out.push_str(&input[pos..cur_match]);
        out.push_str(to);
        pos = cur_match + from.len();
        match find_word_ci_from(input, from, pos) {
            Some(next) => cur_match = next,
            None => {
                out.push_str(&input[pos..]);
                break;
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, Default)]
struct Wave4FaceFlags {
    has_any_candidate: bool,
    has_bargain_keyword: bool,
    has_renown_keyword: bool,
    has_devour_keyword: bool,
    has_gift_keyword: bool,
    has_any_kicker_line: bool,
    has_suspected_text: bool,
    has_monstrosity_or_renown: bool,
    has_energy_text: bool,
    has_monarch_text: bool,
    has_saddle_text: bool,
    has_foretell_text: bool,
    has_blitz_text: bool,
}

impl Wave4FaceFlags {
    fn from_face(norm: &NormalizedFace, has_gift_keyword: bool) -> Self {
        if norm.text.is_empty() && norm.lines.is_empty() {
            return Self {
                has_gift_keyword,
                ..Self::default()
            };
        }
        let mut has_bargain_keyword = false;
        let mut has_renown_keyword = false;
        let mut has_devour_keyword = false;
        let mut has_any_kicker_line = false;
        for l in &norm.lines {
            let b = l.body.trim();
            if !has_bargain_keyword && starts_with_ci(b, "bargain") {
                has_bargain_keyword = true;
            }
            if !has_renown_keyword && starts_with_ci(b, "renown ") {
                has_renown_keyword = true;
            }
            if !has_devour_keyword && starts_with_ci(b, "devour ") {
                has_devour_keyword = true;
            }
            if !has_any_kicker_line
                && (starts_with_ci(b, "kicker") || starts_with_ci(b, "multikicker"))
            {
                has_any_kicker_line = true;
            }
        }
        let bytes = norm.text.as_bytes();
        let len = bytes.len();
        let mut has_any_candidate = false;
        let mut i = 0usize;
        while i < len {
            match bytes[i] {
                b'p' | b'P'
                    if i + 8 <= len && bytes[i..i + 8].eq_ignore_ascii_case(b"prepared") =>
                {
                    has_any_candidate = true;
                    break;
                }
                b's' | b'S' if i + 5 <= len && bytes[i..i + 5].eq_ignore_ascii_case(b"speed") => {
                    has_any_candidate = true;
                    break;
                }
                b'b' | b'B' => {
                    if (i + 8 <= len && bytes[i..i + 8].eq_ignore_ascii_case(b"blessing"))
                        || (has_bargain_keyword
                            && i + 9 <= len
                            && bytes[i..i + 9].eq_ignore_ascii_case(b"bargained"))
                    {
                        has_any_candidate = true;
                        break;
                    }
                }
                b' ' => {
                    if (i + 4 <= len && bytes[i..i + 4].eq_ignore_ascii_case(b" day"))
                        || (i + 6 <= len && bytes[i..i + 6].eq_ignore_ascii_case(b" night"))
                    {
                        has_any_candidate = true;
                        break;
                    }
                }
                b'r' | b'R'
                    if has_renown_keyword
                        && i + 8 <= len
                        && bytes[i..i + 8].eq_ignore_ascii_case(b"renowned") =>
                {
                    has_any_candidate = true;
                    break;
                }
                b'd' | b'D'
                    if has_devour_keyword
                        && i + 8 <= len
                        && bytes[i..i + 8].eq_ignore_ascii_case(b"devoured") =>
                {
                    has_any_candidate = true;
                    break;
                }
                b'g' | b'G'
                    if has_gift_keyword
                        && i + 4 <= len
                        && bytes[i..i + 4].eq_ignore_ascii_case(b"gift") =>
                {
                    has_any_candidate = true;
                    break;
                }
                _ => {}
            }
            i += 1;
        }

        if !has_any_candidate {
            return Self {
                has_any_candidate: false,
                has_bargain_keyword,
                has_renown_keyword,
                has_devour_keyword,
                has_gift_keyword,
                has_any_kicker_line,
                ..Self::default()
            };
        }

        Self {
            has_any_candidate: true,
            has_bargain_keyword,
            has_renown_keyword,
            has_devour_keyword,
            has_gift_keyword,
            has_any_kicker_line,
            has_suspected_text: contains_word_ci(&norm.text, "suspected")
                || contains_word_ci(&norm.text, "suspect"),
            has_monstrosity_or_renown: contains_word_ci(&norm.text, "monstrosity")
                || contains_word_ci(&norm.text, "monstrous")
                || contains_word_ci(&norm.text, "renown")
                || contains_word_ci(&norm.text, "renowned"),
            has_energy_text: norm.text.contains("{E}") || norm.text.contains("{e}"),
            has_monarch_text: contains_word_ci(&norm.text, "monarch"),
            has_saddle_text: contains_word_ci(&norm.text, "saddle")
                || contains_word_ci(&norm.text, "saddled"),
            has_foretell_text: contains_word_ci(&norm.text, "foretell")
                || contains_word_ci(&norm.text, "foretold"),
            has_blitz_text: contains_word_ci(&norm.text, "blitz")
                || contains_word_ci(&norm.text, "blitzed"),
        }
    }
}

fn detect_wave4_rewrites(
    flags: Wave4FaceFlags,
    group: &[NormalizedLine],
) -> Option<Wave4RewriteOpts> {
    if !flags.has_any_candidate {
        return None;
    }
    let joined = if group.len() == 1 {
        std::borrow::Cow::Borrowed(group[0].text.as_str())
    } else {
        std::borrow::Cow::Owned(
            group
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    };
    let s = joined.as_ref();
    let has_prepared = contains_word_ci(s, "prepared") || contains_word_ci(s, "unprepared");
    let bargain = flags.has_bargain_keyword && contains_word_ci(s, "bargained");
    let renown = flags.has_renown_keyword && contains_word_ci(s, "renowned");
    let devour_count = flags.has_devour_keyword
        && (contains_word_ci(s, "for each creature it devoured")
            || contains_word_ci(s, "for each creature ~ devoured"));
    let devour_cond = flags.has_devour_keyword
        && (contains_word_ci(s, "if it devoured a creature")
            || contains_word_ci(s, "if ~ devoured a creature"));
    let becomes_prepared = !flags.has_suspected_text
        && has_prepared
        && (contains_word_ci(s, "become prepared") || contains_word_ci(s, "becomes prepared"));
    let becomes_unprepared = !flags.has_suspected_text
        && has_prepared
        && (contains_word_ci(s, "become unprepared")
            || contains_word_ci(s, "becomes unprepared")
            || contains_word_ci(s, "is no longer prepared"));
    let self_prepared_cond = !flags.has_monstrosity_or_renown
        && !renown
        && !devour_cond
        && has_prepared
        && (contains_word_ci(s, "~ is prepared")
            || contains_word_ci(s, "it is prepared")
            || contains_word_ci(s, "it's prepared")
            || contains_word_ci(s, "~ isn't prepared")
            || contains_word_ci(s, "~ is not prepared")
            || contains_word_ci(s, "it isn't prepared")
            || contains_word_ci(s, "it is not prepared")
            || contains_word_ci(s, "it's not prepared")
            || contains_word_ci(s, "~ is unprepared")
            || contains_word_ci(s, "it is unprepared"));
    let prepared_filter = !flags.has_suspected_text
        && has_prepared
        && [
            "prepared creature",
            "prepared permanent",
            "prepared card",
            "unprepared creature",
            "unprepared permanent",
            "unprepared card",
        ]
        .iter()
        .any(|p| contains_word_ci(s, p));
    let player_speed = !flags.has_energy_text && contains_word_ci(s, "your speed");
    let gift_promised = flags.has_gift_keyword
        && !flags.has_any_kicker_line
        && !bargain
        && (contains_word_ci(s, "if the gift was promised")
            || contains_word_ci(s, "if the gift wasn't promised")
            || contains_word_ci(s, "if the gift was not promised")
            || contains_word_ci(s, "if ~'s gift was promised")
            || contains_word_ci(s, "if ~'s gift wasn't promised")
            || contains_word_ci(s, "if ~'s gift was not promised"));
    let citys_blessing = !flags.has_monarch_text
        && contains_word_ci(s, "the city's blessing")
        && (contains_word_ci(s, "you have the city's blessing")
            || contains_word_ci(s, "you've got the city's blessing")
            || contains_word_ci(s, "you don't have the city's blessing")
            || contains_word_ci(s, "you do not have the city's blessing"));
    let max_speed_cond = !flags.has_saddle_text
        && contains_word_ci(s, "max speed")
        && (contains_word_ci(s, "you have max speed")
            || contains_word_ci(s, "you don't have max speed")
            || contains_word_ci(s, "you do not have max speed"));
    let day_cond = !flags.has_foretell_text
        && (contains_word_ci(s, "it's day")
            || contains_word_ci(s, "it is day")
            || contains_word_ci(s, "it's not day")
            || contains_word_ci(s, "it is not day"));
    let night_cond = !flags.has_blitz_text
        && (contains_word_ci(s, "it's night")
            || contains_word_ci(s, "it is night")
            || contains_word_ci(s, "it's not night")
            || contains_word_ci(s, "it is not night"));
    let opts = Wave4RewriteOpts {
        bargain,
        renown,
        devour_count,
        devour_cond,
        becomes_prepared,
        becomes_unprepared,
        prepared_filter,
        self_prepared_cond,
        player_speed,
        gift_promised,
        citys_blessing,
        max_speed_cond,
        day_cond,
        night_cond,
    };
    if opts == Wave4RewriteOpts::default() {
        None
    } else {
        Some(opts)
    }
}

fn apply_wave4_text_rewrite(s: &str, opts: Wave4RewriteOpts) -> String {
    let mut out = s.to_string();
    if opts.bargain {
        out = replace_word_ci(&out, "bargained", "kicked");
    }
    if opts.renown {
        out = replace_word_ci(&out, "renowned", "monstrous");
    }
    if opts.devour_count {
        out = replace_word_ci(
            &out,
            "for each creature it devoured",
            "for each time it was kicked",
        );
        out = replace_word_ci(
            &out,
            "for each creature ~ devoured",
            "for each time it was kicked",
        );
    }
    if opts.devour_cond {
        out = replace_word_ci(&out, "if it devoured a creature", "if ~ is monstrous");
        out = replace_word_ci(&out, "if ~ devoured a creature", "if ~ is monstrous");
    }
    if opts.becomes_unprepared {
        out = replace_word_ci(&out, "becomes unprepared", "is no longer suspected");
        out = replace_word_ci(&out, "become unprepared", "is no longer suspected");
        out = replace_word_ci(&out, "is no longer prepared", "is no longer suspected");
    }
    if opts.becomes_prepared {
        out = replace_word_ci(&out, "becomes prepared", "becomes suspected");
        out = replace_word_ci(&out, "become prepared", "becomes suspected");
    }
    if opts.self_prepared_cond {
        out = replace_word_ci(&out, "~ isn't prepared", "~ is not monstrous");
        out = replace_word_ci(&out, "~ is not prepared", "~ is not monstrous");
        out = replace_word_ci(&out, "~ is unprepared", "~ is not monstrous");
        out = replace_word_ci(&out, "it isn't prepared", "it is not monstrous");
        out = replace_word_ci(&out, "it is not prepared", "it is not monstrous");
        out = replace_word_ci(&out, "it is unprepared", "it is not monstrous");
        out = replace_word_ci(&out, "it's not prepared", "it's not monstrous");
        out = replace_word_ci(&out, "~ is prepared", "~ is monstrous");
        out = replace_word_ci(&out, "it is prepared", "it is monstrous");
        out = replace_word_ci(&out, "it's prepared", "it's monstrous");
    }
    if opts.prepared_filter {
        for noun in ["creature", "permanent", "card"] {
            out = replace_word_ci(
                &out,
                &format!("unprepared {noun}"),
                &format!("unsuspected {noun}"),
            );
            out = replace_word_ci(
                &out,
                &format!("prepared {noun}"),
                &format!("suspected {noun}"),
            );
        }
    }
    if opts.player_speed {
        out = replace_word_ci(&out, "your speed", "the amount of {E} you have");
    }
    if opts.gift_promised {
        out = replace_word_ci(&out, "if the gift wasn't promised", "if ~ was not kicked");
        out = replace_word_ci(&out, "if the gift was not promised", "if ~ was not kicked");
        out = replace_word_ci(&out, "if ~'s gift wasn't promised", "if ~ was not kicked");
        out = replace_word_ci(&out, "if ~'s gift was not promised", "if ~ was not kicked");
        out = replace_word_ci(&out, "if the gift was promised", "if ~ was kicked");
        out = replace_word_ci(&out, "if ~'s gift was promised", "if ~ was kicked");
    }
    if opts.citys_blessing {
        out = replace_word_ci(
            &out,
            "you don't have the city's blessing",
            "you're not the monarch",
        );
        out = replace_word_ci(
            &out,
            "you do not have the city's blessing",
            "you're not the monarch",
        );
        out = replace_word_ci(&out, "you have the city's blessing", "you're the monarch");
        out = replace_word_ci(&out, "you've got the city's blessing", "you're the monarch");
    }
    if opts.max_speed_cond {
        out = replace_word_ci(&out, "you don't have max speed", "~ is not saddled");
        out = replace_word_ci(&out, "you do not have max speed", "~ is not saddled");
        out = replace_word_ci(&out, "you have max speed", "~ is saddled");
    }
    if opts.day_cond {
        out = replace_word_ci(&out, "it's not day", "~ was not foretold");
        out = replace_word_ci(&out, "it is not day", "~ was not foretold");
        out = replace_word_ci(&out, "it's day", "~ was foretold");
        out = replace_word_ci(&out, "it is day", "~ was foretold");
    }
    if opts.night_cond {
        out = replace_word_ci(&out, "it's not night", "~ was not blitzed");
        out = replace_word_ci(&out, "it is not night", "~ was not blitzed");
        out = replace_word_ci(&out, "it's night", "~ was blitzed");
        out = replace_word_ci(&out, "it is night", "~ was blitzed");
    }
    out
}

fn rewrite_normalized_line_wave4(line: &NormalizedLine, opts: Wave4RewriteOpts) -> NormalizedLine {
    let new_text = apply_wave4_text_rewrite(&line.text, opts);
    let new_body = apply_wave4_text_rewrite(&line.body, opts);
    let start = line.span.start;
    let body_offset = (line.body_span.start - line.span.start) as usize;
    let body_start = start + body_offset.min(new_text.len()) as u32;
    let new_span = Span {
        start,
        end: start + new_text.len() as u32,
    };
    let new_body_span = Span {
        start: body_start,
        end: body_start + new_body.len() as u32,
    };
    let sentences = split_sentences(&new_body, body_start);
    NormalizedLine {
        raw_index: line.raw_index,
        raw: line.raw.clone(),
        text: new_text,
        ability_word: line.ability_word.clone(),
        span: new_span,
        body: new_body,
        body_span: new_body_span,
        sentences,
    }
}

fn map_filter_wave4(filter: &mut ObjectFilter, opts: Wave4RewriteOpts) {
    if opts.prepared_filter {
        if filter.flags.contains(FilterFlags::SUSPECTED) {
            filter.flags.remove(FilterFlags::SUSPECTED);
            filter.flags.insert(FilterFlags::PREPARED);
        }
        if filter.flags.contains(FilterFlags::NOT_SUSPECTED) {
            filter.flags.remove(FilterFlags::NOT_SUSPECTED);
            filter.flags.insert(FilterFlags::NOT_PREPARED);
        }
    }
    for pred in &mut filter.extra {
        if let ObjectPredicate::AnyOf { filters } = pred {
            for sub in filters {
                map_filter_wave4(sub, opts);
            }
        }
    }
}

fn map_amount_wave4(amount: &mut Amount, opts: Wave4RewriteOpts) {
    match amount {
        Amount::KickedCount { multiplier, .. } if opts.devour_count => {
            let mult = multiplier.unwrap_or(1);
            if mult <= 1 {
                *amount = Amount::DevouredCount;
            } else {
                *amount = Amount::Sum {
                    args: (0..mult).map(|_| Amount::DevouredCount).collect(),
                };
            }
        }
        Amount::EnergyCount { player } if opts.player_speed => {
            *amount = Amount::PlayerSpeed {
                player: player.clone(),
            };
        }
        Amount::Count { of } => map_filter_wave4(of, opts),
        Amount::Mul { of, .. } | Amount::Negate { of } | Amount::Half { of, .. } => {
            map_amount_wave4(of, opts);
        }
        Amount::Sum { args } | Amount::Min { args } | Amount::Max { args } => {
            for a in args {
                map_amount_wave4(a, opts);
            }
        }
        Amount::Difference { left, right } => {
            map_amount_wave4(left, opts);
            map_amount_wave4(right, opts);
        }
        Amount::Conditional {
            condition,
            then_amt,
            else_amt,
        } => {
            map_condition_wave4(condition, opts);
            map_amount_wave4(then_amt, opts);
            map_amount_wave4(else_amt, opts);
        }
        _ => {}
    }
}

fn map_condition_wave4(cond: &mut Condition, opts: Wave4RewriteOpts) {
    match cond {
        Condition::Kicked { tag, .. } if opts.bargain && tag.is_none() => {
            *tag = Some("bargain".to_string());
        }
        Condition::Kicked { tag, .. } if opts.gift_promised && tag.is_none() => {
            *cond = Condition::GiftPromised;
        }
        Condition::Monstrous if opts.renown => *cond = Condition::IsRenowned,
        Condition::Monstrous if opts.self_prepared_cond => *cond = Condition::SelfPrepared,
        Condition::Monstrous if opts.devour_cond => {
            *cond = Condition::Compare {
                left: Box::new(Amount::DevouredCount),
                op: CompareOp::Ge,
                right: Box::new(Amount::Const { n: 1 }),
            };
        }
        Condition::IsMonarch { .. } if opts.citys_blessing => {
            *cond = Condition::HasCitysBlessing;
        }
        Condition::Saddled if opts.max_speed_cond => *cond = Condition::HasMaxSpeed,
        Condition::Foretold if opts.day_cond => *cond = Condition::IsDay,
        Condition::Blitzed if opts.night_cond => *cond = Condition::IsNight,
        Condition::Not { of } => map_condition_wave4(of, opts),
        Condition::And { of } | Condition::Or { of } => {
            for c in of {
                map_condition_wave4(c, opts);
            }
        }
        Condition::Compare { left, right, .. } => {
            map_amount_wave4(left, opts);
            map_amount_wave4(right, opts);
        }
        _ => {}
    }
}

fn map_targets_wave4(targets: &mut [mtg_ir::TargetSpec], opts: Wave4RewriteOpts) {
    for t in targets {
        map_filter_wave4(&mut t.filter, opts);
        if let Some(ref mut c) = t.condition {
            map_condition_wave4(c, opts);
        }
        if let Some(ref mut dt) = t.divide_total {
            map_amount_wave4(dt, opts);
        }
    }
}

fn map_step_wave4(step: &mut EffectStep, opts: Wave4RewriteOpts) {
    if let Selector::All { ref mut filter } = step.subject {
        map_filter_wave4(filter, opts);
    }
    if let Some(ref mut amt) = step.amount {
        map_amount_wave4(amt, opts);
    }
    if let Some(StepGate::Condition { ref mut condition })
    | Some(StepGate::Combined {
        condition: Some(ref mut condition),
        ..
    }) = step.gate
    {
        map_condition_wave4(condition, opts);
    }
    if let Some(ref mut rt) = step.reflexive_trigger {
        map_targets_wave4(&mut rt.targets, opts);
        for s in &mut rt.effect {
            map_step_wave4(s, opts);
        }
        if let Some(ref mut m) = rt.modal {
            map_modal_wave4(m, opts);
        }
    }
    match &mut step.op {
        EffectOp::Suspect { unsuspect } => {
            if opts.becomes_unprepared && *unsuspect {
                step.op = EffectOp::Unprepare;
            } else if opts.becomes_prepared && !*unsuspect {
                step.op = EffectOp::Prepare;
            }
        }
        EffectOp::Pump {
            toughness: Some(t), ..
        } => {
            map_amount_wave4(t, opts);
        }
        EffectOp::Pump {
            toughness: None, ..
        } => {}
        EffectOp::Animate {
            set_power,
            set_toughness,
            ..
        } => {
            if let Some(p) = set_power {
                map_amount_wave4(p, opts);
            }
            if let Some(t) = set_toughness {
                map_amount_wave4(t, opts);
            }
        }
        EffectOp::ConditionalSequence {
            condition,
            then_steps,
            else_steps,
        } => {
            map_condition_wave4(condition, opts);
            for s in then_steps {
                map_step_wave4(s, opts);
            }
            for s in else_steps {
                map_step_wave4(s, opts);
            }
        }
        EffectOp::ForEachPlayerLoop { steps, .. } | EffectOp::RepeatSequence { steps, .. } => {
            for s in steps {
                map_step_wave4(s, opts);
            }
        }
        EffectOp::DelayedTrigger {
            delayed: Some(d), ..
        } => {
            for s in &mut d.effect {
                map_step_wave4(s, opts);
            }
        }
        _ => {}
    }
}

fn map_modal_wave4(modal: &mut ModalDef, opts: Wave4RewriteOpts) {
    for m in &mut modal.modes {
        map_targets_wave4(&mut m.targets, opts);
        for s in &mut m.effect {
            map_step_wave4(s, opts);
        }
    }
    for s in &mut modal.prefix_effect {
        map_step_wave4(s, opts);
    }
    for s in &mut modal.suffix_effect {
        map_step_wave4(s, opts);
    }
}

fn map_affected_wave4(aff: &mut Affected, opts: Wave4RewriteOpts) {
    if let Affected::Filter { filter } = aff {
        map_filter_wave4(filter, opts);
    }
}

fn map_ability_wave4(ab: &mut AbilityDef, opts: Wave4RewriteOpts) {
    match ab {
        AbilityDef::Triggered(t) => {
            if opts.renown && t.trigger.event == EventKind::BecameMonstrous {
                t.trigger.event = EventKind::BecameRenowned;
            }
            map_targets_wave4(&mut t.targets, opts);
            if let Some(ref mut c) = t.condition {
                map_condition_wave4(c, opts);
            }
            for s in &mut t.effect {
                map_step_wave4(s, opts);
            }
            if let Some(ref mut m) = t.modal {
                map_modal_wave4(m, opts);
            }
        }
        AbilityDef::Activated(a) => {
            map_targets_wave4(&mut a.targets, opts);
            if let Some(ref mut c) = a.activation_condition {
                map_condition_wave4(c, opts);
            }
            for s in &mut a.effect {
                map_step_wave4(s, opts);
            }
            if let Some(ref mut m) = a.modal {
                map_modal_wave4(m, opts);
            }
        }
        AbilityDef::Spell(sp) => {
            map_targets_wave4(&mut sp.targets, opts);
            for s in &mut sp.effect {
                map_step_wave4(s, opts);
            }
            if let Some(ref mut m) = sp.modal {
                map_modal_wave4(m, opts);
            }
        }
        AbilityDef::Static(st) => {
            if let Some(ref mut c) = st.static_def.condition {
                map_condition_wave4(c, opts);
            }
            match &mut st.static_def.body {
                StaticBody::Continuous {
                    affected, modifier, ..
                } => {
                    map_affected_wave4(affected, opts);
                    match modifier {
                        LayerMod::Pt { power, toughness }
                        | LayerMod::SetBasePt { power, toughness } => {
                            map_amount_wave4(power, opts);
                            map_amount_wave4(toughness, opts);
                        }
                        LayerMod::SetBasePower { power } => map_amount_wave4(power, opts),
                        LayerMod::SetBaseToughness { toughness } => {
                            map_amount_wave4(toughness, opts);
                        }
                        LayerMod::GrantAbility { ability } => {
                            map_ability_wave4(ability, opts);
                        }
                        _ => {}
                    }
                }
                StaticBody::Restriction { affected, .. }
                | StaticBody::CombatRule { affected, .. } => {
                    map_affected_wave4(affected, opts);
                }
                _ => {}
            }
        }
        AbilityDef::Replacement(rep) => {
            if let Some(ref mut c) = rep.replacement.condition {
                map_condition_wave4(c, opts);
            }
        }
        _ => {}
    }
}

fn basic_land_color(subtype_name: &str) -> Option<(ManaColor, &'static str)> {
    match subtype_name {
        "Plains" => Some((ManaColor::W, "mana_w")),
        "Island" => Some((ManaColor::U, "mana_u")),
        "Swamp" => Some((ManaColor::B, "mana_b")),
        "Mountain" => Some((ManaColor::R, "mana_r")),
        "Forest" => Some((ManaColor::G, "mana_g")),
        _ => None,
    }
}

fn build_intrinsic_basic_land_mana(
    characteristics: &Characteristics,
    interner: &Interner,
    mana_tap_taken: &mut bool,
) -> Vec<AbilityDef> {
    if !characteristics.types.contains(CardTypeMask::LAND) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &st_id in &characteristics.subtypes {
        let Some(name) = interner.subtype_name(st_id) else {
            continue;
        };
        let Some((color, alt_id)) = basic_land_color(name) else {
            continue;
        };
        let id = if !*mana_tap_taken {
            *mana_tap_taken = true;
            "mana_tap".to_string()
        } else {
            alt_id.to_string()
        };
        out.push(AbilityDef::Activated(ActivatedAbility {
            id,
            cost: vec![CostDef::TapSelf],
            targets: Vec::new(),
            effect: vec![EffectStep {
                op: EffectOp::AddMana {
                    production: ManaProduction::Fixed {
                        symbols: smallvec![color],
                    },
                    restriction: None,
                    restriction_spec: None,
                    retain_until_eot: false,
                    delayed_rider: None,
                },
                subject: Selector::None,
                actor: None,
                amount: Some(Box::new(Amount::Const { n: 1 })),
                optional: None,
                gate: None,
                bind_as: None,
                do_this_once_per_turn: false,
                reflexive_trigger: None,
                span: Span::default(),
            }],
            modal: None,
            timing: Timing::Mana,
            is_mana: true,
            activation_restrictions: Vec::new(),
            activation_condition: None,
            activator_who: None,
            power_up_reduction: false,
            x_cant_be_zero: false,
            activation_cost_reduction: None,
            activation_zones: ZoneMask::BATTLEFIELD,
            cost_keyword: None,
            intrinsic: true,
            text: None,
            grant_condition: None,
            x_spend_colors: ColorMask::empty(),
            x_colored_only: false,
            x_max_one_per_color: false,
            span: Span::default(),
        }));
    }
    out
}

pub fn compile_faces(
    card: &RawCardInput,
    text_hash: &str,
    interner: &mut Interner,
) -> Result<CardDef, CompileError> {
    let layout = effective_layout(card);
    let candidate_faces = get_card_faces(card);
    let is_supported_multiface = SUPPORTED_MULTIFACE_LAYOUTS.contains(&layout);

    let is_identical_reversible = layout == CardLayout::ReversibleCard
        && candidate_faces.len() == 2
        && candidate_faces[0].name == candidate_faces[1].name
        && candidate_faces[0].mana_cost == candidate_faces[1].mana_cost
        && candidate_faces[0].type_line == candidate_faces[1].type_line
        && candidate_faces[0].oracle_text == candidate_faces[1].oracle_text
        && candidate_faces[0].power == candidate_faces[1].power
        && candidate_faces[0].toughness == candidate_faces[1].toughness;

    let faces: Vec<FaceDef> = if is_identical_reversible {
        let quoted_compiler = CardQuotedAbilityCompiler::default();
        vec![compile_single_face_with_quoted(
            &candidate_faces[0],
            &candidate_faces[..1],
            layout,
            interner,
            &quoted_compiler,
        )?]
    } else if candidate_faces.len() >= 2 && is_supported_multiface {
        let quoted_compiler = CardQuotedAbilityCompiler::default();
        candidate_faces
            .iter()
            .map(|f| {
                compile_single_face_with_quoted(
                    f,
                    &candidate_faces,
                    layout,
                    interner,
                    &quoted_compiler,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
    } else if card.back_face.is_some()
        || card.oracle_text.contains("\n//\n")
        || card.faces.len() > 1
    {
        let top_face = if card.type_line.is_empty() && !candidate_faces.is_empty() {
            candidate_faces[0].clone()
        } else {
            raw_card_as_face(card)
        };
        let characteristics = build_characteristics(&top_face, interner);
        let norm_text = if !card.oracle_text.trim().is_empty() {
            normalize_face(&card.name, &card.oracle_text, false).text
        } else if !candidate_faces.is_empty() {
            candidate_faces
                .iter()
                .map(|f| {
                    let cp = interner.checkpoint();
                    let is_cl = is_character_legend(&build_characteristics(f, interner));
                    interner.rollback(cp);
                    normalize_face(&f.name, &f.oracle_text, is_cl).text
                })
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n//\n")
        } else {
            String::new()
        };
        let span = Span {
            start: 0,
            end: norm_text.len() as u32,
        };
        let unparsed = if span.end > 0 { vec![span] } else { Vec::new() };
        vec![FaceDef {
            name: card.name.clone(),
            characteristics,
            abilities: vec![unsupported_ability(
                UnsupportedReason::Multiface,
                norm_text,
                span,
                unparsed,
            )],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        }]
    } else {
        let single = candidate_faces
            .first()
            .cloned()
            .unwrap_or_else(|| raw_card_as_face(card));
        let quoted_compiler = CardQuotedAbilityCompiler::default();
        vec![compile_single_face_with_quoted(
            &single,
            &candidate_faces,
            layout,
            interner,
            &quoted_compiler,
        )?]
    };

    let fully_supported = faces.iter().all(|f| {
        !f.abilities
            .iter()
            .any(|a| matches!(a, AbilityDef::Unsupported(_)))
    });

    Ok(CardDef {
        oracle_id: card.oracle_id.clone(),
        compiler_version: crate::COMPILER_VERSION,
        text_hash: text_hash.to_string(),
        name: card.name.clone(),
        layout,
        faces,
        fully_supported,
    })
}

fn has_ci_word_pair(s: &str, w1: &str, w2: &str) -> bool {
    let toks = tokenize(s, 0);
    toks.windows(2).any(|w| w[0].eq_ci(w1) && w[1].eq_ci(w2))
}

fn compile_single_face_with_quoted(
    raw_face: &RawFaceInput,
    all_faces: &[RawFaceInput],
    layout: CardLayout,
    interner: &mut Interner,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Result<FaceDef, CompileError> {
    let mut characteristics = build_characteristics(raw_face, interner);
    let is_char_legend = is_character_legend(&characteristics);
    let norm = normalize_face(&raw_face.name, &raw_face.oracle_text, is_char_legend);

    // Battle — Control Point gate
    if characteristics.types.contains(CardTypeMask::BATTLE)
        && has_ci_word_pair(&raw_face.type_line, "control", "point")
    {
        let span = Span {
            start: 0,
            end: norm.text.len() as u32,
        };
        let text = if norm.text.is_empty() {
            raw_face.type_line.clone()
        } else {
            norm.text
        };
        return Ok(FaceDef {
            name: raw_face.name.clone(),
            characteristics,
            abilities: vec![unsupported_ability(
                UnsupportedReason::StaticNotCompiled,
                text,
                span,
                if span.end > 0 { vec![span] } else { Vec::new() },
            )],
            has_read_ahead: false,
            prototype_spec: None,
            cleaved_spell_spec: None,
            gift_promised_spell_spec: None,
        });
    }

    let card_mana_value = if layout == CardLayout::Split && all_faces.len() >= 2 {
        all_faces
            .iter()
            .map(|f| ManaCost::parse(&f.mana_cost).mana_value())
            .sum()
    } else {
        characteristics.mana_cost.mana_value()
    };

    let is_class = characteristics
        .subtypes
        .iter()
        .any(|&id| interner.subtype_name(id) == Some("Class"));
    let is_case = characteristics
        .subtypes
        .iter()
        .any(|&id| interner.subtype_name(id) == Some("Case"));
    let is_spell = characteristics
        .types
        .intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY);
    let is_planeswalker = characteristics.types.contains(CardTypeMask::PLANESWALKER);
    let is_battle = characteristics.types.contains(CardTypeMask::BATTLE);

    let has_cleave_keyword = norm
        .lines
        .iter()
        .any(|l| starts_with_ci(l.body.trim(), "cleave"));
    let has_gift_keyword = norm.lines.iter().any(|l| {
        let b = l.body.trim();
        starts_with_ci(b, "gift a ") || starts_with_ci(b, "gift an ")
    });
    let wave4_flags = Wave4FaceFlags::from_face(&norm, has_gift_keyword);

    let char_snapshot = characteristics.clone();
    let mut ctx = ParseCtx {
        face_name: &raw_face.name,
        characteristics: &char_snapshot,
        interner,
        normalized_text: &norm.text,
        card_mana_value,
        has_read_ahead: false,
        mana_tap_taken: false,
        is_planeswalker,
        is_spell,
        is_class,
        is_case,
        is_battle,
        has_cleave_keyword,
        has_gift_keyword,
        prototype_spec: None,
    };

    let mut abilities: Vec<AbilityDef> = Vec::new();
    let mut spell_lines: Vec<NormalizedLine> = Vec::new();

    // CR 305.6 intrinsic basic-land mana abilities
    abilities.extend(build_intrinsic_basic_land_mana(
        &char_snapshot,
        ctx.interner,
        &mut ctx.mana_tap_taken,
    ));

    let partitioned = partition_striated_lines(&norm, is_class);

    for group in group_lines(&partitioned.base_lines) {
        let line = &group[0];
        if let Some(saga_out) = saga_chapter_line(&mut ctx, &group, quoted_compiler) {
            abilities.extend(saga_out);
            continue;
        }
        if ctx.is_case {
            if let Some(solve_out) = case_to_solve_line(&mut ctx, &group) {
                abilities.extend(solve_out);
                continue;
            }
            if let Some(solved_out) = case_solved_line(&mut ctx, &group) {
                abilities.extend(solved_out);
                continue;
            }
        }
        if let Some(ms_out) = max_speed_line(&mut ctx, &group) {
            abilities.extend(ms_out);
            continue;
        }
        if let Some(inf_out) = infinity_line(&mut ctx, &group) {
            abilities.extend(inf_out);
            continue;
        }
        if !ctx.is_spell
            && let Some(siege_out) = siege_anchor_choice_group(&mut ctx, &group)
        {
            abilities.extend(siege_out);
            continue;
        }
        if ctx.is_spell && (is_spree_group(&group) || is_tiered_group(&group)) {
            spell_lines.extend(group);
            continue;
        }

        let skip_spell_delayed = ctx.is_spell && starts_with_spell_delayed_trigger(&line.body);

        let out = if group.len() == 1 {
            parse_keyword_line(&mut ctx, line)
        } else {
            None
        }
        .or_else(|| {
            if ctx.is_spell {
                None
            } else {
                exert_attack_line(&mut ctx, &group, quoted_compiler)
            }
        })
        .or_else(|| {
            if skip_spell_delayed {
                None
            } else {
                parse_trigger_hook(&mut ctx, &group, wave4_flags, quoted_compiler)
            }
        })
        .or_else(|| activated_line(&mut ctx, &group, wave4_flags, quoted_compiler));

        if let Some(abs) = out {
            abilities.extend(abs);
        } else if ctx.is_spell {
            if group.len() == 1
                && line.ability_word.is_none()
                && !ends_with_spell_sentence_marker(&line.body)
                && !line.text.starts_with('•')
            {
                abilities.push(unsupported_ability(
                    UnsupportedReason::KeywordNotImplemented,
                    line.text.clone(),
                    line.span,
                    vec![line.span],
                ));
            } else {
                spell_lines.extend(group);
            }
        } else {
            abilities.extend(parse_static_hook(&mut ctx, &group, wave4_flags));
        }
    }

    // Leveler striations (CR 711.5)
    for st in &partitioned.level_striations {
        let Some(pt) = &st.pt else {
            let mut band_parts = vec![st.header_text.as_str()];
            let mut unparsed_spans = vec![st.header_span];
            for inner in &st.inner_lines {
                band_parts.push(inner.text.as_str());
                unparsed_spans.push(inner.span);
            }
            let band_end = st
                .inner_lines
                .last()
                .map_or(st.header_span.end, |l| l.span.end.max(st.header_span.end));
            let band_span = Span {
                start: st.header_span.start,
                end: band_end,
            };
            abilities.push(unsupported_ability(
                UnsupportedReason::Unparsed,
                band_parts.join("\n"),
                band_span,
                unparsed_spans,
            ));
            continue;
        };
        let cond = build_level_range_condition(st.min, st.max, ctx.interner);
        if let (Some(p), Some(t)) = (pt.power, pt.toughness) {
            abilities.push(AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer7b,
                        affected: Affected::SelfObj,
                        modifier: LayerMod::SetBasePt {
                            power: Box::new(Amount::Const { n: p }),
                            toughness: Box::new(Amount::Const { n: t }),
                        },
                        active_in_zone: None,
                    },
                    condition: Some(Box::new(cond.clone())),
                },
                span: pt.span,
            }));
        } else {
            let pt_txt = norm
                .text
                .get(pt.span.start as usize..pt.span.end as usize)
                .filter(|s| !s.is_empty())
                .unwrap_or("*")
                .to_string();
            let mut ua =
                unsupported_ability(UnsupportedReason::Unparsed, pt_txt, pt.span, vec![pt.span]);
            if let AbilityDef::Unsupported(ref mut u) = ua {
                u.grant_condition = Some(Box::new(cond.clone()));
            }
            abilities.push(ua);
        }
        for inner_group in group_lines(&st.inner_lines) {
            for ab in compile_permanent_group(&mut ctx, &inner_group) {
                abilities.extend(gate_ability_by_striation_condition(
                    ab,
                    cond.clone(),
                    &norm.text,
                    ctx.interner,
                ));
            }
        }
    }

    // Station striations (CR 722.2)
    if !partitioned.station_striations.is_empty() {
        let is_non_creature = !characteristics.types.contains(CardTypeMask::CREATURE);
        let printed_pt = characteristics.pt;
        let has_printed_station_pt = is_non_creature && printed_pt.is_some();
        let default_creature_threshold = find_raw_creature_at_threshold(&raw_face.oracle_text)
            .unwrap_or_else(|| {
                partitioned
                    .station_striations
                    .iter()
                    .map(|s| s.threshold)
                    .max()
                    .unwrap_or(0)
            });
        let mut animated_via_station = false;

        for st in &partitioned.station_striations {
            let cond = build_station_counter_condition(st.threshold, ctx.interner);
            let is_implicit = st.explicit_pt.is_none()
                && has_printed_station_pt
                && st.threshold == default_creature_threshold;
            if st.explicit_pt.is_some() || is_implicit {
                animated_via_station = true;
                abilities.push(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer4,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::AddTypes {
                                types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                                subtypes: SmallVec::new(),
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond.clone())),
                    },
                    span: st.header_span,
                }));
                let resolved_pt = if let Some((ep, et)) = st.explicit_pt {
                    Some((ep, et))
                } else if let Some(PrintedPt::Fixed { power, toughness }) = printed_pt {
                    Some((power, toughness))
                } else if let Some(PrintedPt::Cda {
                    power: Some(p),
                    toughness: Some(t),
                }) = printed_pt
                {
                    Some((p, t))
                } else {
                    None
                };
                if let Some((p, t)) = resolved_pt {
                    abilities.push(AbilityDef::Static(StaticAbility {
                        static_def: StaticDef {
                            body: StaticBody::Continuous {
                                layer: Layer::Layer7b,
                                affected: Affected::SelfObj,
                                modifier: LayerMod::SetBasePt {
                                    power: Box::new(Amount::Const { n: p }),
                                    toughness: Box::new(Amount::Const { n: t }),
                                },
                                active_in_zone: None,
                            },
                            condition: Some(Box::new(cond.clone())),
                        },
                        span: st.header_span,
                    }));
                }
            }
            for inner_group in group_lines(&st.inner_lines) {
                for ab in compile_permanent_group(&mut ctx, &inner_group) {
                    abilities.extend(gate_ability_by_striation_condition(
                        ab,
                        cond.clone(),
                        &norm.text,
                        ctx.interner,
                    ));
                }
            }
        }

        // If reminder text declared `creature at N+` without a matching `N+` striation band,
        // synthesize the Layer 4 AddTypes + Layer 7b SetBasePt static at `default_creature_threshold`.
        if has_printed_station_pt && !animated_via_station && default_creature_threshold > 0 {
            let cond = build_station_counter_condition(default_creature_threshold, ctx.interner);
            let fallback_span = partitioned
                .station_striations
                .first()
                .map_or(Span::default(), |s| s.header_span);
            abilities.push(AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::Continuous {
                        layer: Layer::Layer4,
                        affected: Affected::SelfObj,
                        modifier: LayerMod::AddTypes {
                            types: CardTypeMask::ARTIFACT | CardTypeMask::CREATURE,
                            subtypes: SmallVec::new(),
                        },
                        active_in_zone: None,
                    },
                    condition: Some(Box::new(cond.clone())),
                },
                span: fallback_span,
            }));
            let resolved_pt = match printed_pt {
                Some(PrintedPt::Fixed { power, toughness }) => Some((power, toughness)),
                Some(PrintedPt::Cda {
                    power: Some(p),
                    toughness: Some(t),
                }) => Some((p, t)),
                _ => None,
            };
            if let Some((p, t)) = resolved_pt {
                abilities.push(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer7b,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::SetBasePt {
                                power: Box::new(Amount::Const { n: p }),
                                toughness: Box::new(Amount::Const { n: t }),
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond)),
                    },
                    span: fallback_span,
                }));
            }
        }

        // CR 722.2c: Non-creature Station cards do not have printed power/toughness outside the station symbol.
        if is_non_creature {
            characteristics.pt = None;
        }
    }

    // Class striations (CR 717.2)
    for st in &partitioned.class_striations {
        let cost = parse_cost_list(&st.cost_text, st.cost_span.start, &mut ctx);
        let cost_ok = cost.unparsed.is_empty()
            && !cost.defs.is_empty()
            && cost.defs.iter().all(|d| {
                !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
            });
        let level_id = format!("class_level_{}", st.level);
        let activate_cond = Condition::ClassLevelIs {
            level: st.level - 1,
        };
        if !cost_ok {
            let unparsed = if cost.unparsed.is_empty() {
                vec![st.cost_span]
            } else {
                cost.unparsed
            };
            abilities.push(AbilityDef::Unsupported(UnsupportedAbility {
                reason: UnsupportedReason::Unparsed,
                text: st.header_text.clone(),
                span: st.header_span,
                unparsed,
                id: Some(level_id),
                cost: cost.defs,
                cost_complete: Some(false),
                cost_keyword: None,
                timing: Some(Timing::Sorcery),
                is_mana: false,
                effect: Vec::new(),
                targets: Vec::new(),
                activation_restrictions: Vec::new(),
                activation_condition: Some(Box::new(activate_cond)),
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                cast_restrictions: Vec::new(),
                cast_condition: None,
                trigger: None,
                condition: None,
                grant_condition: None,
            }));
        } else {
            abilities.push(AbilityDef::Activated(ActivatedAbility {
                id: level_id,
                cost: cost.defs,
                targets: Vec::new(),
                effect: Vec::new(),
                modal: None,
                timing: Timing::Sorcery,
                is_mana: false,
                activation_restrictions: Vec::new(),
                activation_condition: Some(Box::new(activate_cond)),
                activator_who: None,
                power_up_reduction: false,
                x_cant_be_zero: false,
                activation_cost_reduction: None,
                activation_zones: ZoneMask::BATTLEFIELD,
                cost_keyword: None,
                intrinsic: false,
                text: Some(st.header_text.clone()),
                grant_condition: None,
                x_spend_colors: ColorMask::empty(),
                x_colored_only: false,
                x_max_one_per_color: false,
                span: st.header_span,
            }));
        }
        let gate_cond = Condition::ClassLevelAtLeast { level: st.level };
        for inner_group in group_lines(&st.inner_lines) {
            for ab in compile_permanent_group(&mut ctx, &inner_group) {
                abilities.extend(gate_ability_by_striation_condition(
                    ab,
                    gate_cond.clone(),
                    &norm.text,
                    ctx.interner,
                ));
            }
        }
    }

    // Spell lines (Cleave, Gift, or standard/Spree/Tiered spell compilation)
    let mut cleaved_spell_spec: Option<Box<SpellAbility>> = None;
    let mut gift_promised_spell_spec: Option<Box<SpellAbility>> = None;
    if !spell_lines.is_empty() {
        if ctx.has_cleave_keyword {
            let (spell_out, cleaved, cleave_ok) = compile_cleave_spell_bundle(
                &mut ctx,
                &norm,
                &spell_lines,
                wave4_flags,
                quoted_compiler,
            );
            abilities.extend(spell_out);
            cleaved_spell_spec = cleaved.map(Box::new);
            if !cleave_ok {
                for ab in &mut abilities {
                    if let AbilityDef::Keyword(ka) = ab
                        && ctx.interner.keyword_name(ka.keyword.keyword) == Some("cleave")
                    {
                        let txt = norm
                            .text
                            .get(ka.span.start as usize..ka.span.end as usize)
                            .unwrap_or("")
                            .to_string();
                        *ab = unsupported_ability(
                            UnsupportedReason::Unparsed,
                            txt,
                            ka.span,
                            vec![ka.span],
                        );
                    }
                }
            }
        } else if ctx.has_gift_keyword {
            let (spell_out, promised) = compile_gift_spell_bundle(
                &mut ctx,
                &norm,
                &spell_lines,
                wave4_flags,
                quoted_compiler,
            );
            abilities.extend(spell_out);
            gift_promised_spell_spec = promised.map(Box::new);
        } else {
            let spell_out =
                parse_spell_hook(&mut ctx, &norm, &spell_lines, wave4_flags, quoted_compiler);
            abilities.extend(spell_out);
        }
    }

    // CR 702.165b: Backup post-pass
    for (idx, ab) in abilities.iter_mut().enumerate() {
        if let AbilityDef::Keyword(ka) = ab
            && ctx.interner.keyword_name(ka.keyword.keyword) == Some("backup")
        {
            ka.keyword.backup_ability_start_index = Some((idx + 1) as u16);
        }
    }

    // `granted_N` IDs post-pass
    let mut grant_idx = 0usize;
    for ab in &mut abilities {
        if let AbilityDef::Static(StaticAbility {
            static_def:
                StaticDef {
                    body:
                        StaticBody::Continuous {
                            modifier: LayerMod::GrantAbility { ability },
                            ..
                        },
                    ..
                },
            ..
        }) = ab
            && let AbilityDef::Activated(act) = ability.as_mut()
        {
            act.id = format!("granted_{grant_idx}");
            grant_idx += 1;
        }
    }

    Ok(FaceDef {
        name: raw_face.name.clone(),
        characteristics,
        abilities,
        has_read_ahead: ctx.has_read_ahead,
        prototype_spec: ctx.prototype_spec,
        cleaved_spell_spec,
        gift_promised_spell_spec,
    })
}

// ---- Striations ------------------------------------------------------------------------------

struct LevelPt {
    power: Option<i32>,
    toughness: Option<i32>,
    span: Span,
}

struct LevelStriation {
    min: i32,
    max: Option<i32>,
    header_text: String,
    header_span: Span,
    pt: Option<LevelPt>,
    inner_lines: Vec<NormalizedLine>,
}

struct StationStriation {
    threshold: i32,
    header_span: Span,
    explicit_pt: Option<(i32, i32)>,
    inner_lines: Vec<NormalizedLine>,
}

struct ClassStriation {
    level: u8,
    cost_text: String,
    cost_span: Span,
    header_text: String,
    header_span: Span,
    inner_lines: Vec<NormalizedLine>,
}

struct PartitionedLines {
    base_lines: Vec<NormalizedLine>,
    level_striations: Vec<LevelStriation>,
    station_striations: Vec<StationStriation>,
    class_striations: Vec<ClassStriation>,
}

fn find_raw_creature_at_threshold(oracle_text: &str) -> Option<i32> {
    let toks = tokenize(oracle_text, 0);
    for i in 0..toks.len().saturating_sub(3) {
        if toks[i].eq_ci("creature")
            && toks[i + 1].eq_ci("at")
            && let TokenKind::Number(n) = toks[i + 2].kind
            && toks[i + 3].is_punct(Punct::Plus)
        {
            return Some(n as i32);
        }
    }
    None
}

fn parse_pt_token(s: &str) -> Option<(Option<i32>, Option<i32>)> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(trimmed);
    let slash = inner.find('/')?;
    let p_str = inner[..slash].trim();
    let t_str = inner[slash + 1..].trim();
    let p_ok = p_str == "*" || p_str.parse::<i32>().is_ok();
    let t_ok = t_str == "*" || t_str.parse::<i32>().is_ok();
    if !p_ok || !t_ok {
        return None;
    }
    Some((p_str.parse::<i32>().ok(), t_str.parse::<i32>().ok()))
}

fn parse_numeric_pt_only(s: &str) -> Option<(i32, i32)> {
    let trimmed = s.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(trimmed);
    let slash = inner.find('/')?;
    let p = inner[..slash].trim().parse::<i32>().ok()?;
    let t = inner[slash + 1..].trim().parse::<i32>().ok()?;
    Some((p, t))
}

fn parse_class_header(line: &NormalizedLine) -> Option<(u8, String, Span)> {
    let trimmed = line.text.trim_end();
    // Scan backwards over digits
    let digit_start = trimmed
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + trimmed[i..].chars().next().unwrap().len_utf8())
        .unwrap_or(0);
    let digits = &trimmed[digit_start..];
    if digits.is_empty() {
        return None;
    }
    let level: u8 = digits.parse().ok()?;
    if level < 2 {
        return None;
    }
    let before_digits = trimmed[..digit_start].trim_end();
    if before_digits.len() < "level".len() {
        return None;
    }
    let level_pos = before_digits.len() - "level".len();
    if !before_digits[level_pos..].eq_ignore_ascii_case("level") {
        return None;
    }
    let before_level = before_digits[..level_pos].trim_end();
    let cost_raw = before_level.strip_suffix(':')?;
    let cost_trimmed = cost_raw.trim();
    if cost_trimmed.is_empty() {
        return None;
    }
    let rel_start = line.text.find(cost_trimmed).unwrap_or(0);
    let cost_span = Span {
        start: line.span.start + rel_start as u32,
        end: line.span.start + (rel_start + cost_trimmed.len()) as u32,
    };
    Some((level, cost_trimmed.to_string(), cost_span))
}

type LevelHeader = (i32, Option<i32>, Option<(Option<i32>, Option<i32>)>);

fn parse_level_header(text: &str) -> Option<LevelHeader> {
    let t = text.trim();
    if !starts_with_ci(t, "level") {
        return None;
    }
    let after_kw = &t["level".len()..];
    if !after_kw.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = after_kw.trim_start();
    let dlen = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if dlen == 0 {
        return None;
    }
    let min: i32 = rest[..dlen].parse().ok()?;
    let mut rem = &rest[dlen..];
    let max = if let Some(after_plus) = rem.strip_prefix('+') {
        rem = after_plus;
        None
    } else {
        let r2 = rem.trim_start();
        let dash = r2.chars().next()?;
        if !matches!(dash, '-' | '–' | '—') {
            return None;
        }
        let r3 = r2[dash.len_utf8()..].trim_start();
        let d2 = r3.bytes().take_while(|b| b.is_ascii_digit()).count();
        if d2 == 0 {
            return None;
        }
        let m: i32 = r3[..d2].parse().ok()?;
        rem = &r3[d2..];
        Some(m)
    };

    let rem_trimmed = rem.trim_start();
    if rem_trimmed.is_empty() {
        return Some((min, max, None));
    }
    if !rem.starts_with(char::is_whitespace) {
        return None;
    }
    let pt_part = rem_trimmed
        .strip_prefix('|')
        .unwrap_or(rem_trimmed)
        .trim_start();
    let pt = parse_pt_token(pt_part)?;
    Some((min, max, Some(pt)))
}

fn parse_station_header(text: &str, allow_bare: bool) -> Option<(i32, Option<usize>)> {
    let t = text.trim();
    if starts_with_ci(t, "station") && t["station".len()..].starts_with(char::is_whitespace) {
        let rest = t["station".len()..].trim_start();
        let dlen = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
        if dlen > 0 && rest[dlen..].starts_with('+') {
            let threshold: i32 = rest[..dlen].parse().ok()?;
            let after_plus = rest[dlen + 1..].trim_start();
            if after_plus.is_empty() {
                return Some((threshold, None));
            }
            if after_plus.starts_with('|') {
                return Some((threshold, t.find('|')));
            }
        }
    }
    // Variant B: `N+ | ...` or (when inside a Station card) bare `N+`
    let dlen = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if dlen > 0 && t[dlen..].starts_with('+') {
        let after_plus = t[dlen + 1..].trim_start();
        if after_plus.starts_with('|') {
            let threshold: i32 = t[..dlen].parse().ok()?;
            return Some((threshold, t.find('|')));
        }
        if allow_bare && after_plus.is_empty() {
            let threshold: i32 = t[..dlen].parse().ok()?;
            return Some((threshold, None));
        }
    }
    None
}

fn partition_striated_lines(norm: &NormalizedFace, is_class: bool) -> PartitionedLines {
    let mut base_lines = Vec::new();
    let mut level_striations = Vec::new();
    let mut station_striations = Vec::new();
    let mut class_striations = Vec::new();

    let has_station = norm.lines.iter().any(|l| {
        l.body.trim().eq_ignore_ascii_case("station")
            || parse_station_header(&l.text, false).is_some()
    });

    let total = norm.lines.len();
    let mut i = 0usize;
    while i < total {
        let line = &norm.lines[i];
        if is_class && let Some((level, cost_text, cost_span)) = parse_class_header(line) {
            let mut inner_lines = Vec::new();
            i += 1;
            while i < total {
                let cand = &norm.lines[i];
                if parse_class_header(cand).is_some() {
                    break;
                }
                inner_lines.push(cand.clone());
                i += 1;
            }
            class_striations.push(ClassStriation {
                level,
                cost_text,
                cost_span,
                header_text: line.text.clone(),
                header_span: line.span,
                inner_lines,
            });
            continue;
        }

        if let Some((min, max, inline_pt)) = parse_level_header(&line.text) {
            let mut pt = inline_pt.map(|(power, toughness)| LevelPt {
                power,
                toughness,
                span: line.span,
            });
            if pt.is_none()
                && i + 1 < total
                && let Some((power, toughness)) = parse_pt_token(&norm.lines[i + 1].text)
            {
                pt = Some(LevelPt {
                    power,
                    toughness,
                    span: norm.lines[i + 1].span,
                });
                i += 1;
            }
            let mut inner_lines = Vec::new();
            i += 1;
            while i < total {
                let cand = &norm.lines[i];
                if parse_level_header(&cand.text).is_some()
                    || (has_station && parse_station_header(&cand.text, true).is_some())
                {
                    break;
                }
                inner_lines.push(cand.clone());
                i += 1;
            }
            level_striations.push(LevelStriation {
                min,
                max,
                header_text: line.text.clone(),
                header_span: line.span,
                pt,
                inner_lines,
            });
            continue;
        }

        if has_station && let Some((threshold, _)) = parse_station_header(&line.text, true) {
            let mut explicit_pt: Option<(i32, i32)> = None;
            let mut inner_lines = Vec::new();

            if let Some(pipe_idx) = line.text.find('|') {
                let after_pipe_raw = &line.text[pipe_idx + 1..];
                let lead_ws = after_pipe_raw.len() - after_pipe_raw.trim_start().len();
                let mut after_pipe = after_pipe_raw.trim_start();
                let body_start = line.span.start + pipe_idx as u32 + 1 + lead_ws as u32;
                let trimmed_end = after_pipe.trim_end();
                if trimmed_end.ends_with(']')
                    && let Some(open_br) = trimmed_end.rfind('[')
                    && let Some((p, t)) = parse_numeric_pt_only(&trimmed_end[open_br..])
                {
                    explicit_pt = Some((p, t));
                    after_pipe = trimmed_end[..open_br].trim_end();
                }
                let trimmed_right = after_pipe.trim_end();
                if !trimmed_right.is_empty() {
                    let sub_span = Span {
                        start: body_start,
                        end: body_start + trimmed_right.len() as u32,
                    };
                    inner_lines.push(NormalizedLine {
                        raw_index: line.raw_index,
                        raw: line.raw.clone(),
                        text: trimmed_right.to_string(),
                        ability_word: None,
                        span: sub_span,
                        body: trimmed_right.to_string(),
                        body_span: sub_span,
                        sentences: split_sentences(trimmed_right, body_start),
                    });
                }
            }

            i += 1;
            while i < total {
                let cand = &norm.lines[i];
                if parse_station_header(&cand.text, true).is_some()
                    || parse_level_header(&cand.text).is_some()
                {
                    break;
                }
                if explicit_pt.is_none()
                    && let Some((p, t)) = parse_numeric_pt_only(&cand.text)
                {
                    explicit_pt = Some((p, t));
                } else {
                    inner_lines.push(cand.clone());
                }
                i += 1;
            }
            station_striations.push(StationStriation {
                threshold,
                header_span: line.span,
                explicit_pt,
                inner_lines,
            });
            continue;
        }

        base_lines.push(line.clone());
        i += 1;
    }

    PartitionedLines {
        base_lines,
        level_striations,
        station_striations,
        class_striations,
    }
}

fn build_level_range_condition(min: i32, max: Option<i32>, interner: &mut Interner) -> Condition {
    let level_id = interner.intern_counter("level");
    let ge = Condition::Compare {
        left: Box::new(Amount::CountersOn {
            counter: CounterTarget::Specific(level_id),
            of: mtg_ir::Ref::SelfObj,
        }),
        op: CompareOp::Ge,
        right: Box::new(Amount::Const { n: min }),
    };
    let Some(m) = max else {
        return ge;
    };
    Condition::And {
        of: vec![
            ge,
            Condition::Compare {
                left: Box::new(Amount::CountersOn {
                    counter: CounterTarget::Specific(level_id),
                    of: mtg_ir::Ref::SelfObj,
                }),
                op: CompareOp::Le,
                right: Box::new(Amount::Const { n: m }),
            },
        ],
    }
}

fn build_station_counter_condition(min_charge: i32, interner: &mut Interner) -> Condition {
    let charge_id = interner.intern_counter("charge");
    Condition::Compare {
        left: Box::new(Amount::CountersOn {
            counter: CounterTarget::Specific(charge_id),
            of: mtg_ir::Ref::SelfObj,
        }),
        op: CompareOp::Ge,
        right: Box::new(Amount::Const { n: min_charge }),
    }
}

fn and_conditions(existing: Option<Box<Condition>>, added: Condition) -> Box<Condition> {
    match existing {
        None => Box::new(added),
        Some(mut b) => {
            if let Condition::And { of } = b.as_mut() {
                of.push(added);
                b
            } else {
                Box::new(Condition::And {
                    of: vec![*b, added],
                })
            }
        }
    }
}

fn compile_permanent_group(ctx: &mut ParseCtx<'_>, group: &[NormalizedLine]) -> Vec<AbilityDef> {
    let quoted = CardQuotedAbilityCompiler::default();
    let wave4_flags = Wave4FaceFlags::default();
    if let Some(saga) = saga_chapter_line(ctx, group, &quoted) {
        return saga;
    }
    if group.len() == 1
        && let Some(kw) = parse_keyword_line(ctx, &group[0])
    {
        return kw;
    }
    if let Some(trig) = parse_trigger_hook(ctx, group, wave4_flags, &quoted) {
        return trig;
    }
    if let Some(act) = activated_line(ctx, group, wave4_flags, &quoted) {
        return act;
    }
    parse_static_hook(ctx, group, wave4_flags)
}

fn gate_ability_by_striation_condition(
    ab: AbilityDef,
    cond: Condition,
    norm_text: &str,
    interner: &mut Interner,
) -> Vec<AbilityDef> {
    match ab {
        AbilityDef::Keyword(ka) => {
            let kw = ka.keyword;
            let kw_name = interner.keyword_name(kw.keyword).unwrap_or("").to_string();
            let spec = get_keyword_spec(&kw_name);
            if spec.is_some_and(|s| s.has_display_label)
                && kw.param.is_none()
                && kw.cost.is_none()
                && kw.n.is_none()
            {
                return vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec![kw.keyword],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond)),
                    },
                    span: ka.span,
                })];
            }
            if kw_name == "hexproof"
                && let Some(p) = &kw.param
            {
                let g_id =
                    interner.intern_keyword(&format!("hexproof from {}", p.to_ascii_lowercase()));
                return vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec![g_id],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond)),
                    },
                    span: ka.span,
                })];
            }
            if kw_name == "protection"
                && let Some(p) = &kw.param
            {
                let g_id =
                    interner.intern_keyword(&format!("protection from {}", p.to_ascii_lowercase()));
                return vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec![g_id],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond)),
                    },
                    span: ka.span,
                })];
            }
            if kw_name == "ward" && (kw.cost.is_some() || kw.param.is_some()) {
                let ward_str = if let Some(c) = &kw.cost {
                    format!("Ward {}", c.raw)
                } else {
                    format!("Ward—{}", kw.param.as_deref().unwrap_or(""))
                };
                let g_id = interner.intern_keyword(&ward_str);
                return vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec![g_id],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond)),
                    },
                    span: ka.span,
                })];
            }
            if matches!(
                kw_name.as_str(),
                "toxic" | "afflict" | "firebending" | "mobilize" | "rampage" | "renown"
            ) && let Some(n) = kw.n
            {
                let g_id = interner.intern_keyword(&format!("{kw_name} {n}"));
                return vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Continuous {
                            layer: Layer::Layer6,
                            affected: Affected::SelfObj,
                            modifier: LayerMod::AddKeywords {
                                keywords: smallvec![g_id],
                                protection_attachment_exemption: None,
                            },
                            active_in_zone: None,
                        },
                        condition: Some(Box::new(cond)),
                    },
                    span: ka.span,
                })];
            }
            let _ = spec.map(|s| s.impl_kind == KeywordImplKind::Static);
            let txt = norm_text
                .get(ka.span.start as usize..ka.span.end as usize)
                .unwrap_or("")
                .to_string();
            vec![unsupported_ability(
                UnsupportedReason::StaticNotCompiled,
                txt,
                ka.span,
                vec![ka.span],
            )]
        }
        AbilityDef::Static(mut sa) => {
            sa.static_def.condition = Some(and_conditions(sa.static_def.condition, cond));
            vec![AbilityDef::Static(sa)]
        }
        AbilityDef::Replacement(mut ra) => {
            ra.replacement.condition = Some(and_conditions(ra.replacement.condition, cond));
            vec![AbilityDef::Replacement(ra)]
        }
        AbilityDef::Triggered(mut ta) => {
            ta.grant_condition = Some(and_conditions(ta.grant_condition, cond));
            vec![AbilityDef::Triggered(ta)]
        }
        AbilityDef::Activated(mut aa) => {
            aa.grant_condition = Some(and_conditions(aa.grant_condition, cond));
            vec![AbilityDef::Activated(aa)]
        }
        AbilityDef::Unsupported(mut ua) => {
            ua.grant_condition = Some(and_conditions(ua.grant_condition, cond));
            vec![AbilityDef::Unsupported(ua)]
        }
        other => vec![other],
    }
}

// ---- Grouping & Structural Ability Forms -----------------------------------------------------

fn starts_with_pawprint_dash(s: &str) -> bool {
    let mut rest = s.trim_start();
    let mut count = 0usize;
    while let Some(after) = rest
        .strip_prefix("{P}")
        .or_else(|| rest.strip_prefix("{p}"))
    {
        count += 1;
        rest = after;
    }
    count > 0 && rest.trim_start().starts_with('—')
}

pub fn group_lines(lines: &[NormalizedLine]) -> Vec<Vec<NormalizedLine>> {
    let mut groups: Vec<Vec<NormalizedLine>> = Vec::new();
    for line in lines {
        let attach = if let Some(prev) = groups.last() {
            line.text.starts_with('•')
                || starts_with_pawprint_dash(line.text.trim())
                || (prev[0].body.trim().eq_ignore_ascii_case("spree")
                    && line
                        .text
                        .trim()
                        .strip_prefix('+')
                        .is_some_and(|r| r.starts_with(char::is_whitespace)))
        } else {
            false
        };
        if attach {
            groups.last_mut().unwrap().push(line.clone());
        } else {
            groups.push(vec![line.clone()]);
        }
    }
    groups
}

fn group_span(group: &[NormalizedLine]) -> Span {
    Span {
        start: group[0].span.start,
        end: group.last().unwrap().span.end,
    }
}

fn saga_roman_to_int(r: &str) -> Option<u8> {
    match r {
        "I" => Some(1),
        "II" => Some(2),
        "III" => Some(3),
        "IV" => Some(4),
        "V" => Some(5),
        "VI" => Some(6),
        _ => None,
    }
}

fn modal_interpretable(parse: &EffectParse, is_triggered: bool) -> bool {
    let Some(ref modal) = parse.modal else {
        return false;
    };
    if modal.modes.is_empty() || modal.max < modal.min {
        return false;
    }
    if !modal.prefix_effect.is_empty() {
        let sub = EffectParse {
            steps: modal.prefix_effect.clone(),
            targets: Vec::new(),
            modal: None,
            unparsed: Vec::new(),
        };
        if !statically_interpretable(&sub, is_triggered) {
            return false;
        }
    }
    if !modal.suffix_effect.is_empty() {
        let sub = EffectParse {
            steps: modal.suffix_effect.clone(),
            targets: Vec::new(),
            modal: None,
            unparsed: Vec::new(),
        };
        if !statically_interpretable(&sub, is_triggered) {
            return false;
        }
    }
    modal.modes.iter().all(|m| {
        let sub = EffectParse {
            steps: m.effect.clone(),
            targets: m.targets.clone(),
            modal: None,
            unparsed: Vec::new(),
        };
        if is_triggered {
            interpretable_trigger(&sub)
        } else {
            statically_interpretable(&sub, false)
        }
    })
}

fn saga_chapter_line(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    let line = &group[0];
    let dash_pos = line.text.find('—')?;
    let prefix = line.text[..dash_pos].trim();
    if prefix.is_empty() {
        return None;
    }
    let mut chapter_numbers: SmallVec<[u8; 3]> = SmallVec::new();
    for part in prefix.split(',') {
        let n = saga_roman_to_int(part.trim())?;
        chapter_numbers.push(n);
    }
    if chapter_numbers.is_empty() {
        return None;
    }

    let span = group_span(group);
    let full_text: String = group
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let after_dash = &line.text[dash_pos + '—'.len_utf8()..];
    let lead_ws = after_dash.len() - after_dash.trim_start().len();
    let mut effect_start = line.span.start + (dash_pos + '—'.len_utf8() + lead_ws) as u32;
    let mut header_effect_text = after_dash.trim_start();

    // Optional post-chapter flavor word: `I — Gungnir — Destroy...`
    if let Some(sep_pos) = header_effect_text.find(" — ") {
        let flavor = &header_effect_text[..sep_pos];
        let after_sep = &header_effect_text[sep_pos + " — ".len()..];
        let flavor_ok = !flavor.is_empty()
            && flavor.as_bytes()[0].is_ascii_uppercase()
            && flavor.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '\'' | '’' | ' ' | '!' | '?' | '-')
            })
            && tokenize(flavor, 0)
                .first()
                .is_none_or(|t| !t.eq_ci("choose"));
        let next_ok = after_sep
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || matches!(c, '•' | '{' | '~'));
        if flavor_ok && next_ok {
            effect_start += (sep_pos + " — ".len()) as u32;
            header_effect_text = after_sep;
        }
    }

    let header_sentences = split_sentences(header_effect_text, effect_start);
    let mut only_once_per_turn = false;
    if header_sentences.len() >= 2 {
        let last = header_sentences.last().unwrap();
        let trimmed = last.text.trim();
        let no_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
        if no_dot.eq_ignore_ascii_case("this ability triggers only once each turn") {
            only_once_per_turn = true;
            let cut = (last.span.start - effect_start) as usize;
            header_effect_text = header_effect_text[..cut].trim_end();
        }
    }

    let lore_id = ctx.interner.intern_counter("lore");
    let trigger = TriggerDef {
        event: EventKind::CounterPlaced,
        filter: EventFilter {
            object: Some(TriggerSubject::SelfObj),
            detail: Some(EventDetail::Counter {
                counter: Some(lore_id),
                last_counter: false,
                put_by_player: None,
                nth_counter_on_self: None,
            }),
            ..Default::default()
        },
        scope: None,
        from_zones: None,
        only_once_per_turn,
        do_this_once_per_turn: false,
        is_state_trigger: false,
        or: Vec::new(),
        chapter_numbers,
        is_chapter_ability: true,
        spell_delayed: None,
    };

    if group.len() > 1 {
        let bullets: Vec<ModalBullet<'_>> = group[1..]
            .iter()
            .map(|b| ModalBullet {
                text: &b.text,
                span: b.span,
            })
            .collect();
        let expanded = parse_expanded_modal_with_quoted(
            header_effect_text,
            &bullets,
            AbilityContext::Triggered,
            effect_start,
            Some(&trigger),
            ctx,
            quoted_compiler,
        );
        let modal_parse = if let Some(exp) = expanded {
            exp
        } else if starts_with_ci(header_effect_text.trim_start(), "choose") {
            parse_modal_with_quoted(
                header_effect_text,
                &bullets,
                AbilityContext::Triggered,
                effect_start,
                Some(&trigger),
                ctx,
                quoted_compiler,
            )
        } else {
            EffectParse::default()
        };
        let modes_ok = modal_parse.modal.is_some()
            && modal_parse.unparsed.is_empty()
            && modal_interpretable(&modal_parse, true);
        if !modes_ok {
            let gaps = if !modal_parse.unparsed.is_empty() {
                modal_parse.unparsed
            } else {
                vec![Span {
                    start: effect_start,
                    end: span.end,
                }]
            };
            let mut ua = unsupported_ability(UnsupportedReason::Unparsed, full_text, span, gaps);
            if let AbilityDef::Unsupported(ref mut u) = ua {
                u.trigger = Some(trigger);
            }
            return Some(vec![ua]);
        }
        return Some(vec![AbilityDef::Triggered(TriggeredAbility {
            trigger,
            condition: None,
            grant_condition: None,
            targets: Vec::new(),
            effect: Vec::new(),
            modal: modal_parse.modal,
            text: Some(full_text),
            span,
        })]);
    }

    let effect = parse_effects_with_quoted(
        header_effect_text,
        effect_start,
        AbilityContext::Triggered,
        Some(&trigger),
        ctx,
        quoted_compiler,
    );
    if !interpretable_trigger(&effect) {
        let gaps = if !effect.unparsed.is_empty() {
            effect.unparsed
        } else {
            vec![Span {
                start: effect_start,
                end: span.end,
            }]
        };
        let mut ua = unsupported_ability(UnsupportedReason::Unparsed, full_text, span, gaps);
        if let AbilityDef::Unsupported(ref mut u) = ua {
            u.trigger = Some(trigger);
        }
        return Some(vec![ua]);
    }
    Some(vec![AbilityDef::Triggered(TriggeredAbility {
        trigger,
        condition: None,
        grant_condition: None,
        targets: effect.targets,
        effect: effect.steps,
        modal: effect.modal,
        text: Some(full_text),
        span,
    })])
}

fn parse_case_solve_condition(raw_text: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let s = raw_text.trim().trim_end_matches('.').trim();
    if s.is_empty() {
        return None;
    }
    let toks = tokenize(s, 0);
    let mut cur = crate::lexer::TokenCursor::new(&toks);

    // 1. "you've gained N or more life this turn"
    if cur.eat_ci("you've")
        && cur.eat_ci("gained")
        && let Some(n) = cur.bump().and_then(|t| parse_small_number(t.raw))
        && cur.eat_ci("or")
        && cur.eat_ci("more")
        && cur.eat_ci("life")
        && cur.eat_ci("this")
        && cur.eat_ci("turn")
        && cur.is_empty()
    {
        return Some(Condition::GainedLifeThisTurn {
            min: Some(Box::new(Amount::Const { n })),
        });
    }
    cur.restore(0);

    // 2. "you have no cards in hand"
    if toks.len() == 6
        && toks[0].eq_ci("you")
        && toks[1].eq_ci("have")
        && toks[2].eq_ci("no")
        && toks[3].eq_ci("cards")
        && toks[4].eq_ci("in")
        && toks[5].eq_ci("hand")
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Hand,
                of: PlayerRef::You,
            }),
            op: CompareOp::Eq,
            right: Box::new(Amount::Const { n: 0 }),
        });
    }

    // 3. "there are N or more cards in your graveyard"
    if toks.len() == 9
        && toks[0].eq_ci("there")
        && toks[1].eq_ci("are")
        && let Some(n) = parse_small_number(toks[2].raw)
        && toks[3].eq_ci("or")
        && toks[4].eq_ci("more")
        && toks[5].eq_ci("cards")
        && toks[6].eq_ci("in")
        && toks[7].eq_ci("your")
        && toks[8].eq_ci("graveyard")
    {
        return Some(Condition::Compare {
            left: Box::new(Amount::CardsIn {
                zone: Zone::Graveyard,
                of: PlayerRef::You,
            }),
            op: CompareOp::Ge,
            right: Box::new(Amount::Const { n }),
        });
    }

    // 4. "N or more creatures attacked this turn"
    if toks.len() == 7
        && let Some(n) = parse_small_number(toks[0].raw)
        && toks[1].eq_ci("or")
        && toks[2].eq_ci("more")
        && toks[3].eq_ci("creatures")
        && toks[4].eq_ci("attacked")
        && toks[5].eq_ci("this")
        && toks[6].eq_ci("turn")
    {
        return Some(Condition::CreaturesAttackedCountThisTurn { min: n as u16 });
    }

    // 5. "creatures you control have total power N or greater"
    if toks.len() == 9
        && toks[0].eq_ci("creatures")
        && toks[1].eq_ci("you")
        && toks[2].eq_ci("control")
        && toks[3].eq_ci("have")
        && toks[4].eq_ci("total")
        && toks[5].eq_ci("power")
        && let Some(n) = parse_small_number(toks[6].raw)
        && toks[7].eq_ci("or")
        && toks[8].eq_ci("greater")
    {
        return Some(Condition::TotalPowerControlled { min: n });
    }

    // 6. "there are N [or more] colors among permanents you control"
    if toks.len() >= 8
        && toks[0].eq_ci("there")
        && toks[1].eq_ci("are")
        && let Some(n) = parse_small_number(toks[2].raw)
    {
        let tail = if toks.get(3).is_some_and(|t| t.eq_ci("or"))
            && toks.get(4).is_some_and(|t| t.eq_ci("more"))
        {
            &toks[5..]
        } else {
            &toks[3..]
        };
        if tail.len() == 5
            && tail[0].eq_ci("colors")
            && tail[1].eq_ci("among")
            && tail[2].eq_ci("permanents")
            && tail[3].eq_ci("you")
            && tail[4].eq_ci("control")
        {
            return Some(Condition::ColorsAmongControlledPermanents { min: n as u8 });
        }
    }

    // 7. "N or more sources you controlled dealt damage this turn"
    if toks.len() == 10
        && let Some(n) = parse_small_number(toks[0].raw)
        && toks[1].eq_ci("or")
        && toks[2].eq_ci("more")
        && toks[3].eq_ci("sources")
        && toks[4].eq_ci("you")
        && toks[5].eq_ci("controlled")
        && toks[6].eq_ci("dealt")
        && toks[7].eq_ci("damage")
        && toks[8].eq_ci("this")
        && toks[9].eq_ci("turn")
    {
        return Some(Condition::SourcesDealtDamageThisTurn { min: n as u16 });
    }

    // 8. "N or more <qual> cards were put into (your graveyard|graveyards) from anywhere this turn"
    if toks.len() >= 10
        && let Some(n) = parse_small_number(toks[0].raw)
        && toks[1].eq_ci("or")
        && toks[2].eq_ci("more")
        && let Some(were_pos) = toks.iter().position(|t| t.eq_ci("were"))
        && were_pos > 3
        && (toks[were_pos - 1].eq_ci("card") || toks[were_pos - 1].eq_ci("cards"))
    {
        let tail = &toks[were_pos..];
        let (whose, rest_idx) = if tail.len() >= 7
            && tail[1].eq_ci("put")
            && tail[2].eq_ci("into")
            && tail[3].eq_ci("your")
            && tail[4].eq_ci("graveyard")
        {
            (Some(RelativeControl::You), 5)
        } else if tail.len() >= 6
            && tail[1].eq_ci("put")
            && tail[2].eq_ci("into")
            && tail[3].eq_ci("graveyards")
        {
            (Some(RelativeControl::Any), 4)
        } else {
            (None, 0)
        };
        if let Some(whose) = whose
            && tail[rest_idx..].len() == 4
            && tail[rest_idx].eq_ci("from")
            && tail[rest_idx + 1].eq_ci("anywhere")
            && tail[rest_idx + 2].eq_ci("this")
            && tail[rest_idx + 3].eq_ci("turn")
        {
            let qual =
                s[toks[3].span.start as usize..toks[were_pos - 1].span.start as usize].trim();
            let filter = if qual.is_empty() {
                None
            } else {
                Some(Box::new(parse_multi_or_object_phrase(
                    qual,
                    ctx,
                    FilterOpts::default(),
                )?))
            };
            return Some(Condition::CardsPutIntoGraveyardFromAnywhereThisTurn {
                min: n as u16,
                filter,
                whose: Some(whose),
            });
        }
    }

    // 9. "you've cast N or more [<qual>] spells this turn"
    if toks.len() >= 7
        && toks[0].eq_ci("you've")
        && toks[1].eq_ci("cast")
        && let Some(n) = parse_small_number(toks[2].raw)
        && toks[3].eq_ci("or")
        && toks[4].eq_ci("more")
        && toks[toks.len() - 3].eq_ci("spells")
        && toks[toks.len() - 2].eq_ci("this")
        && toks[toks.len() - 1].eq_ci("turn")
    {
        let qual_toks = &toks[5..toks.len() - 3];
        let filter = if qual_toks.is_empty() {
            None
        } else {
            let qual = s
                [qual_toks[0].span.start as usize..qual_toks.last().unwrap().span.end as usize]
                .trim();
            if qual.eq_ignore_ascii_case("instant and sorcery") {
                Some(Box::new(ObjectFilter {
                    types_any: CardTypeMask::INSTANT | CardTypeMask::SORCERY,
                    ..Default::default()
                }))
            } else {
                Some(Box::new(parse_multi_or_object_phrase(
                    qual,
                    ctx,
                    FilterOpts::default(),
                )?))
            }
        };
        return Some(Condition::SpellsCastThisTurnAtLeast {
            min: n as u16,
            filter,
        });
    }

    // 10. "you control no <qual>"
    if toks.len() >= 4 && toks[0].eq_ci("you") && toks[1].eq_ci("control") && toks[2].eq_ci("no") {
        let qual = s[toks[3].span.start as usize..].trim();
        let mut f = parse_multi_or_object_phrase(qual, ctx, FilterOpts::default())?;
        f.zones = ZoneMask::BATTLEFIELD;
        f.controller = Some(RelativeControl::You);
        return Some(Condition::Compare {
            left: Box::new(Amount::Count { of: Box::new(f) }),
            op: CompareOp::Eq,
            right: Box::new(Amount::Const { n: 0 }),
        });
    }

    // 11. "you control N or more <qual>"
    if toks.len() >= 6
        && toks[0].eq_ci("you")
        && toks[1].eq_ci("control")
        && let Some(n) = parse_small_number(toks[2].raw)
        && toks[3].eq_ci("or")
        && toks[4].eq_ci("more")
    {
        let qual = s[toks[5].span.start as usize..].trim();
        let mut f = parse_multi_or_object_phrase(qual, ctx, FilterOpts::default())?;
        f.zones = ZoneMask::BATTLEFIELD;
        f.controller = Some(RelativeControl::You);
        return Some(Condition::Compare {
            left: Box::new(Amount::Count { of: Box::new(f) }),
            op: CompareOp::Ge,
            right: Box::new(Amount::Const { n }),
        });
    }

    // 12. "you control [a|an] <qual>"
    if toks.len() >= 3 && toks[0].eq_ci("you") && toks[1].eq_ci("control") {
        let start_idx = if toks[2].eq_ci("a") || toks[2].eq_ci("an") {
            3
        } else {
            2
        };
        if start_idx < toks.len() {
            let qual = s[toks[start_idx].span.start as usize..].trim();
            let mut f = parse_multi_or_object_phrase(qual, ctx, FilterOpts::default())?;
            f.zones = ZoneMask::BATTLEFIELD;
            f.controller = Some(RelativeControl::You);
            return Some(Condition::Compare {
                left: Box::new(Amount::Count { of: Box::new(f) }),
                op: CompareOp::Ge,
                right: Box::new(Amount::Const { n: 1 }),
            });
        }
    }

    None
}

fn case_to_solve_line(ctx: &mut ParseCtx<'_>, group: &[NormalizedLine]) -> Option<Vec<AbilityDef>> {
    if group.len() != 1 {
        return None;
    }
    let line = &group[0];
    if !starts_with_ci(&line.text, "to solve") {
        return None;
    }
    let after = line.text["to solve".len()..].trim_start();
    let cond_raw = after.strip_prefix('—')?.trim();
    let Some(solve_cond) = parse_case_solve_condition(cond_raw, ctx) else {
        return Some(vec![unsupported_ability(
            UnsupportedReason::Unparsed,
            line.text.clone(),
            line.span,
            vec![line.span],
        )]);
    };
    Some(vec![AbilityDef::Triggered(TriggeredAbility {
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
            scope: Some(TriggerScope::ControllerTurn),
            from_zones: None,
            only_once_per_turn: false,
            do_this_once_per_turn: false,
            is_state_trigger: false,
            or: Vec::new(),
            chapter_numbers: SmallVec::new(),
            is_chapter_ability: false,
            spell_delayed: None,
        },
        condition: Some(Box::new(Condition::And {
            of: vec![Condition::CaseUnsolved, solve_cond],
        })),
        grant_condition: None,
        targets: Vec::new(),
        effect: vec![EffectStep {
            op: EffectOp::SolveCase,
            subject: Selector::SelfObj,
            actor: None,
            amount: None,
            optional: None,
            gate: None,
            bind_as: None,
            do_this_once_per_turn: false,
            reflexive_trigger: None,
            span: line.span,
        }],
        modal: None,
        text: Some(line.text.clone()),
        span: line.span,
    })])
}

fn strip_prefix_dash_and_gate(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    prefix_kw: &str,
    cond: Condition,
) -> Option<Vec<AbilityDef>> {
    let first = &group[0];
    if !starts_with_ci(&first.text, prefix_kw) {
        return None;
    }
    let after_kw = &first.text[prefix_kw.len()..];
    let after_ws = after_kw.trim_start();
    let after_dash = after_ws.strip_prefix('—')?.trim_start();
    let prefix_len = first.text.len() - after_dash.len();
    let inner_start = first.span.start + prefix_len as u32;
    let inner_span = Span {
        start: inner_start,
        end: first.span.end,
    };
    let stripped_first = NormalizedLine {
        raw_index: first.raw_index,
        raw: first.raw.clone(),
        text: after_dash.to_string(),
        ability_word: None,
        span: inner_span,
        body: after_dash.to_string(),
        body_span: inner_span,
        sentences: split_sentences(after_dash, inner_start),
    };
    let mut inner_group = Vec::with_capacity(group.len());
    inner_group.push(stripped_first);
    inner_group.extend(group[1..].iter().cloned());
    let norm_text = ctx.normalized_text.to_string();
    let compiled = compile_permanent_group(ctx, &inner_group);
    let mut out = Vec::new();
    for ab in compiled {
        out.extend(gate_ability_by_striation_condition(
            ab,
            cond.clone(),
            &norm_text,
            ctx.interner,
        ));
    }
    Some(out)
}

fn case_solved_line(ctx: &mut ParseCtx<'_>, group: &[NormalizedLine]) -> Option<Vec<AbilityDef>> {
    strip_prefix_dash_and_gate(ctx, group, "solved", Condition::CaseSolved)
}

fn max_speed_line(ctx: &mut ParseCtx<'_>, group: &[NormalizedLine]) -> Option<Vec<AbilityDef>> {
    strip_prefix_dash_and_gate(ctx, group, "max speed", Condition::HasMaxSpeed)
}

fn infinity_line(ctx: &mut ParseCtx<'_>, group: &[NormalizedLine]) -> Option<Vec<AbilityDef>> {
    strip_prefix_dash_and_gate(ctx, group, "∞", Condition::IsHarnessed)
}

fn siege_anchor_choice_group(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
) -> Option<Vec<AbilityDef>> {
    if group.len() < 2 {
        return None;
    }
    let header = group[0].body.trim().trim_end_matches('.').trim();
    let toks = tokenize(header, 0);
    // "As ~ enters[ the battlefield], choose <WordA> or <WordB>"
    let mut cur = crate::lexer::TokenCursor::new(&toks);
    if !cur.eat_ci("as")
        || !cur
            .bump()
            .is_some_and(|t| matches!(t.kind, TokenKind::SelfRef))
        || !cur.eat_ci("enters")
    {
        return None;
    }
    let _ = cur.eat_ci("the") && cur.eat_ci("battlefield");
    if !cur.eat_punct(Punct::Comma) || !cur.eat_ci("choose") {
        return None;
    }
    let word_a = cur.bump()?.raw.to_string();
    if !cur.eat_ci("or") {
        return None;
    }
    let word_b = cur.bump()?.raw.to_string();
    if !cur.is_empty() || word_a.eq_ignore_ascii_case(&word_b) {
        return None;
    }

    // Validate all bullet lines: `• <word> — <inner>`
    let mut parsed_bullets: Vec<(String, String, usize, &NormalizedLine)> = Vec::new();
    for b_line in &group[1..] {
        let after_bullet = b_line.text.strip_prefix('•')?.trim_start();
        let dash = after_bullet.find('—')?;
        let bw = after_bullet[..dash].trim();
        let canonical = if bw.eq_ignore_ascii_case(&word_a) {
            word_a.clone()
        } else if bw.eq_ignore_ascii_case(&word_b) {
            word_b.clone()
        } else {
            return None;
        };
        let inner = after_bullet[dash + '—'.len_utf8()..].trim_start();
        let inner_offset = b_line.text.len() - inner.len();
        parsed_bullets.push((canonical, inner.to_string(), inner_offset, b_line));
    }

    let mut out = Vec::new();
    out.push(AbilityDef::Replacement(ReplacementAbility {
        replacement: ReplacementDef {
            replaces: EventPattern {
                event: EventKind::ZoneChange,
                filter: EventFilter {
                    object: Some(TriggerSubject::SelfObj),
                    detail: Some(EventDetail::ZoneChange {
                        from: ZoneMask::empty(),
                        to: Some(Zone::Battlefield),
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
                    etb_choices: vec![EtbChoiceSpec::AnchorWord {
                        words: [word_a, word_b],
                    }],
                    ..Default::default()
                }),
            },
            self_replacement: true,
            who: None,
            condition: None,
        },
        span: group[0].span,
    }));

    let norm_text = ctx.normalized_text.to_string();
    for (canonical, inner_text, inner_offset, orig_line) in parsed_bullets {
        let inner_norm = normalize_face("~", &inner_text, false);
        let base_start = orig_line.span.start + inner_offset as u32;
        let shifted_lines: Vec<NormalizedLine> = inner_norm
            .lines
            .into_iter()
            .map(|mut l| {
                l.raw_index = orig_line.raw_index;
                l.span.start += base_start;
                l.span.end += base_start;
                l.body_span.start += base_start;
                l.body_span.end += base_start;
                for s in &mut l.sentences {
                    s.span.start += base_start;
                    s.span.end += base_start;
                }
                l
            })
            .collect();
        let cond = Condition::AnchorWordChosen { word: canonical };
        for ab in compile_permanent_group(ctx, &shifted_lines) {
            out.extend(gate_ability_by_striation_condition(
                ab,
                cond.clone(),
                &norm_text,
                ctx.interner,
            ));
        }
    }
    Some(out)
}

fn is_spree_group(group: &[NormalizedLine]) -> bool {
    group[0].body.trim().eq_ignore_ascii_case("spree")
}

fn is_tiered_group(group: &[NormalizedLine]) -> bool {
    group[0].body.trim().eq_ignore_ascii_case("tiered")
}

fn starts_with_spell_delayed_trigger(body: &str) -> bool {
    let t = body.trim();
    starts_with_ci(t, "at the beginning of the next")
        || starts_with_ci(t, "at the beginning of that turn's")
        || starts_with_ci(t, "at the end of combat")
        || starts_with_ci(t, "at end of combat")
}

fn ends_with_spell_sentence_marker(body: &str) -> bool {
    body.trim_end()
        .chars()
        .next_back()
        .is_some_and(|c| matches!(c, '.' | '!' | '—' | '"' | '”' | ']' | '~'))
}

// ---- Activated Abilities & Cost Colon --------------------------------------------------------

pub fn cost_colon(body: &str) -> Option<usize> {
    let mut in_double = false;
    let mut in_single = false;
    let mut iter = body.char_indices().peekable();
    let mut prev_ch: Option<char> = None;

    while let Some((idx, ch)) = iter.next() {
        if !in_single && matches!(ch, '"' | '“' | '”') {
            in_double = match ch {
                '“' => true,
                '”' => false,
                _ => !in_double,
            };
        } else if !in_double && matches!(ch, '\'' | '‘' | '’') {
            let can_open = prev_ch
                .is_none_or(|p| p.is_whitespace() || matches!(p, '(' | '[' | '{' | '"' | '“'));
            if !in_single && can_open {
                in_single = true;
            } else if in_single {
                let next_alnum = iter.peek().is_some_and(|&(_, nc)| nc.is_alphanumeric());
                if !next_alnum {
                    in_single = false;
                }
            }
        } else if !in_double && !in_single && ch == ':' && idx > 0 {
            return Some(idx);
        }
        prev_ch = Some(ch);
    }
    None
}

fn parse_exert_attack_body(body: &str) -> Option<(bool, Option<&str>)> {
    let t = body.trim();
    let mut has_not_exerted_cond = false;
    let mut rest = t;
    if starts_with_ci(rest, "if ") {
        let after_if = rest[3..].trim_start();
        let after_subj = ["~ ", "it ", "he ", "she ", "they "].iter().find_map(|p| {
            if starts_with_ci(after_if, p) {
                Some(after_if[p.len()..].trim_start())
            } else {
                None
            }
        })?;
        let after_neg = [
            "hasn't ",
            "hasn’t ",
            "wasn't ",
            "wasn’t ",
            "has not ",
            "was not ",
        ]
        .iter()
        .find_map(|p| {
            if starts_with_ci(after_subj, p) {
                Some(after_subj[p.len()..].trim_start())
            } else {
                None
            }
        })?;
        let after_been = if starts_with_ci(after_neg, "been ") {
            after_neg[5..].trim_start()
        } else {
            after_neg
        };
        let after_cond = ["exerted this turn,", "exerted this turn ,"]
            .iter()
            .find_map(|p| {
                if starts_with_ci(after_been, p) {
                    Some(after_been[p.len()..].trim_start())
                } else {
                    None
                }
            })?;
        has_not_exerted_cond = true;
        rest = after_cond;
    }

    if !starts_with_ci(rest, "you may exert ") {
        return None;
    }
    let after_exert = rest["you may exert ".len()..].trim_start();
    let after_obj = ["~ ", "it ", "him ", "her ", "them "]
        .iter()
        .find_map(|p| {
            if starts_with_ci(after_exert, p) {
                Some(after_exert[p.len()..].trim_start())
            } else {
                None
            }
        })?;
    if !starts_with_ci(after_obj, "as ") {
        return None;
    }
    let after_as = after_obj[3..].trim_start();
    let after_subj2 = ["~ ", "it ", "he ", "she ", "they "].iter().find_map(|p| {
        if starts_with_ci(after_as, p) {
            Some(after_as[p.len()..].trim_start())
        } else {
            None
        }
    })?;
    if !starts_with_ci(after_subj2, "attacks.") {
        return None;
    }
    let after_dot = after_subj2["attacks.".len()..].trim();
    if after_dot.is_empty() {
        return Some((has_not_exerted_cond, None));
    }
    if starts_with_ci(after_dot, "when you do,") {
        let follow = after_dot["when you do,".len()..].trim();
        if !follow.is_empty() {
            return Some((has_not_exerted_cond, Some(follow)));
        }
    }
    None
}

fn shift_step_gate(step: &mut EffectStep) {
    match &mut step.gate {
        None => {
            step.gate = Some(StepGate::IfPrecedingAccepted { step_index: 0 });
        }
        Some(StepGate::IfPrecedingAccepted { step_index })
        | Some(StepGate::IfPrecedingDeclined { step_index }) => {
            *step_index += 1;
        }
        Some(StepGate::Combined {
            if_preceding_accepted,
            if_preceding_declined,
            ..
        }) => {
            if let Some(a) = if_preceding_accepted {
                *a += 1;
            } else if if_preceding_declined.is_none() {
                *if_preceding_accepted = Some(0);
            }
            if let Some(d) = if_preceding_declined {
                *d += 1;
            }
        }
        Some(StepGate::Condition { .. }) => {}
    }
}

fn exert_attack_line(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    if group.len() != 1 {
        return None;
    }
    let line = &group[0];
    let (has_cond, follow_opt) = parse_exert_attack_body(&line.body)?;
    let span = group_span(group);
    let full_text = line.text.clone();
    let trig = TriggerDef {
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
    let condition = has_cond.then(|| {
        Box::new(Condition::Not {
            of: Box::new(Condition::ExertedThisTurn),
        })
    });
    let exert_step = EffectStep {
        op: EffectOp::Exert,
        subject: Selector::SelfObj,
        actor: None,
        amount: None,
        optional: Some(OptionalSpec {
            chooser: PlayerRef::You,
            cost: Vec::new(),
            mode: OptionalMode::DoIfPaid,
        }),
        gate: None,
        bind_as: None,
        do_this_once_per_turn: false,
        reflexive_trigger: None,
        span: line.body_span,
    };

    let Some(follow_text) = follow_opt else {
        return Some(vec![AbilityDef::Triggered(TriggeredAbility {
            trigger: trig,
            condition,
            grant_condition: None,
            targets: Vec::new(),
            effect: vec![exert_step],
            modal: None,
            text: Some(full_text),
            span,
        })]);
    };

    let follow_start = line
        .body_span
        .end
        .saturating_sub(follow_text.len() as u32)
        .max(line.body_span.start);
    let follow_eff = parse_effects_with_quoted(
        follow_text,
        follow_start,
        AbilityContext::Triggered,
        Some(&trig),
        ctx,
        quoted_compiler,
    );
    if follow_eff.steps.is_empty()
        || !follow_eff.unparsed.is_empty()
        || !statically_interpretable(&follow_eff, true)
    {
        let gaps = if !follow_eff.unparsed.is_empty() {
            follow_eff.unparsed
        } else {
            vec![Span {
                start: follow_start,
                end: span.end,
            }]
        };
        let mut ua = unsupported_ability(UnsupportedReason::Unparsed, full_text, span, gaps);
        if let AbilityDef::Unsupported(ref mut u) = ua {
            u.trigger = Some(trig);
            u.condition = condition;
        }
        return Some(vec![ua]);
    }

    if !follow_eff.targets.is_empty() {
        let mut refl_step = exert_step;
        refl_step.reflexive_trigger = Some(Box::new(ReflexiveTriggerSpec {
            targets: follow_eff.targets,
            effect: follow_eff.steps,
            modal: None,
            text: Some(format!("When you do, {follow_text}")),
            event_filter: Some(ReflexiveEventFilter::WhenYouDo),
            object_filter: None,
        }));
        return Some(vec![AbilityDef::Triggered(TriggeredAbility {
            trigger: trig,
            condition,
            grant_condition: None,
            targets: Vec::new(),
            effect: vec![refl_step],
            modal: None,
            text: Some(full_text),
            span,
        })]);
    }

    let mut steps = Vec::with_capacity(1 + follow_eff.steps.len());
    steps.push(exert_step);
    for mut s in follow_eff.steps {
        shift_step_gate(&mut s);
        steps.push(s);
    }
    Some(vec![AbilityDef::Triggered(TriggeredAbility {
        trigger: trig,
        condition,
        grant_condition: None,
        targets: Vec::new(),
        effect: steps,
        modal: None,
        text: Some(full_text),
        span,
    })])
}

struct ActivatedRestrictionsResult {
    restrictions: Vec<ActivationRestriction>,
    timing: Timing,
    x_cant_be_zero: bool,
    activator_who: Option<ActivatorWho>,
    activation_condition: Option<Condition>,
    explicit_zones: Option<ZoneMask>,
    activation_cost_reduction: Option<ConditionalCostReduction>,
    cleaned_effect_for_parse: String,
    cleaned_modal_header: String,
}

fn push_unique_activation_restriction(
    out: &mut Vec<ActivationRestriction>,
    r: ActivationRestriction,
) {
    if !out.contains(&r) {
        out.push(r);
    }
}

fn push_unique_cast_restriction(
    out: &mut Vec<SpellCastRestrictionKind>,
    r: SpellCastRestrictionKind,
) {
    if !out.contains(&r) {
        out.push(r);
    }
}

fn split_only_clause_parts(body: &str) -> Vec<String> {
    let low = body.to_ascii_lowercase();
    let bytes = low.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if low[i..].starts_with(", and only ") {
            let seg = body[start..i].trim();
            if !seg.is_empty() {
                parts.push(seg.to_string());
            }
            i += ", and only ".len();
            start = i;
            continue;
        }
        if low[i..].starts_with(" and only ") {
            let seg = body[start..i].trim();
            if !seg.is_empty() {
                parts.push(seg.to_string());
            }
            i += " and only ".len();
            start = i;
            continue;
        }
        if bytes[i] == b',' {
            let after = low[i + 1..].trim_start();
            if after.starts_with("as a sorcery")
                || after.starts_with("once")
                || after.starts_with("before")
                || after.starts_with("after")
                || after.starts_with("during")
                || after.starts_with("if")
            {
                let seg = body[start..i].trim();
                if !seg.is_empty() {
                    parts.push(seg.to_string());
                }
                let ws = low[i + 1..].len() - after.len();
                i = i + 1 + ws;
                start = i;
                continue;
            }
        }
        if let Some(ch) = low[i..].chars().next() {
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    let tail = body[start..].trim();
    if !tail.is_empty() {
        parts.push(tail.to_string());
    }
    parts
}

fn parse_restriction_if_condition(cond_str: &str, ctx: &mut ParseCtx<'_>) -> Option<Condition> {
    let norm = cond_str
        .trim()
        .trim_end_matches('.')
        .trim()
        .replace('’', "'");
    let low = norm.to_ascii_lowercase();
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
    if low == "~ is prepared" || low == "it is prepared" || low == "it's prepared" {
        return Some(Condition::SelfPrepared);
    }
    if low == "~ isn't prepared"
        || low == "~ is not prepared"
        || low == "~ is unprepared"
        || low == "it isn't prepared"
        || low == "it is not prepared"
        || low == "it is unprepared"
        || low == "it's not prepared"
    {
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
    let cp = ctx.interner.checkpoint();
    let toks = tokenize(norm.as_str(), 0);
    if !toks.is_empty() {
        let mut scope = NullScope;
        let mut sub_ctx = ctx.with_normalized_text(norm.as_str());
        if let Some((cond, consumed)) = parse_condition(&toks, &mut sub_ctx, &mut scope)
            && consumed == toks.len()
        {
            return Some(cond);
        }
    }
    ctx.interner.rollback(cp);
    None
}

type ActivateOnlyResult = (
    bool,
    Vec<ActivationRestriction>,
    Vec<Condition>,
    Option<ZoneMask>,
);

fn parse_activate_only_sentence(
    sentence_text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ActivateOnlyResult> {
    let trimmed = sentence_text.trim().trim_end_matches('.').trim();
    let head = if starts_with_ci(trimmed, "activate only ") {
        trimmed["activate only ".len()..].trim()
    } else {
        return None;
    };
    if head.is_empty() {
        return None;
    }

    let parts = split_only_clause_parts(head);
    if parts.is_empty() {
        return None;
    }

    let cp = ctx.interner.checkpoint();
    let mut sorcery = false;
    let mut restrictions = Vec::new();
    let mut conditions = Vec::new();
    let mut explicit_zones: Option<ZoneMask> = None;

    for part in parts {
        let mut p = part.trim().to_string();
        if p.to_ascii_lowercase().starts_with("only ") {
            p = p["only ".len()..].trim().to_string();
        }
        let p_ascii_low = p.to_ascii_lowercase();
        let opp_suf_len = if p_ascii_low.ends_with(" on an opponent's turn") {
            Some(" on an opponent's turn".len())
        } else if p_ascii_low.ends_with(" on an opponent’s turn") {
            Some(" on an opponent’s turn".len())
        } else {
            None
        };
        if let Some(suf_len) = opp_suf_len
            && p.len() > suf_len
        {
            push_unique_activation_restriction(
                &mut restrictions,
                ActivationRestriction::DuringOpponentsTurn,
            );
            let cut = p.len() - suf_len;
            p = p[..cut].trim().to_string();
        } else if p_ascii_low.ends_with(" on your turn") && p.len() > " on your turn".len() {
            push_unique_activation_restriction(
                &mut restrictions,
                ActivationRestriction::DuringYourTurn,
            );
            let cut = p.len() - " on your turn".len();
            p = p[..cut].trim().to_string();
        }

        let lower = p.replace('’', "'").to_ascii_lowercase();
        match lower.as_str() {
            "as a sorcery" | "any time you could cast a sorcery" => {
                sorcery = true;
            }
            "as an instant" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::OnlyAsInstant,
                );
            }
            "once each turn" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::OnceEachTurn,
                );
            }
            "once" | "once each game" | "once per game" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::OncePerGame,
                );
            }
            "during your turn" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringYourTurn,
                );
            }
            "during an opponent's turn"
            | "during opponents' turns"
            | "during turns other than yours" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringOpponentsTurn,
                );
            }
            "during your upkeep" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringYourUpkeep,
                );
            }
            "during combat" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringCombat,
                );
            }
            "before attackers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::BeforeAttackers,
                );
            }
            "during combat before attackers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringCombat,
                );
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::BeforeAttackers,
                );
            }
            "after attackers are declared" | "during the declare attackers step" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::AfterAttackers,
                );
            }
            "during combat after attackers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringCombat,
                );
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::AfterAttackers,
                );
            }
            "before blockers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::BeforeBlockers,
                );
            }
            "during combat before blockers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringCombat,
                );
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::BeforeBlockers,
                );
            }
            "during the declare blockers step" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringBlockers,
                );
            }
            "after blockers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::AfterBlockers,
                );
            }
            "during combat after blockers are declared" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringCombat,
                );
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::AfterBlockers,
                );
            }
            "after combat" | "during the postcombat main phase" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::AfterCombat,
                );
            }
            "during your postcombat main phase" => {
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::DuringYourTurn,
                );
                push_unique_activation_restriction(
                    &mut restrictions,
                    ActivationRestriction::AfterCombat,
                );
            }
            "if ~ is in your hand" => {
                explicit_zones = Some(ZoneMask::HAND);
            }
            "if ~ is in your graveyard" => {
                explicit_zones = Some(ZoneMask::GRAVEYARD);
            }
            _ => {
                if lower.starts_with("if ") {
                    let cond_slice = p["if ".len()..].trim();
                    if let Some(cond) = parse_restriction_if_condition(cond_slice, ctx) {
                        conditions.push(cond);
                        continue;
                    }
                }
                ctx.interner.rollback(cp);
                return None;
            }
        }
    }

    Some((sorcery, restrictions, conditions, explicit_zones))
}

fn parse_spell_cast_restriction_sentence(
    sentence_text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<(Vec<SpellCastRestrictionKind>, Option<Condition>)> {
    let trimmed = sentence_text.trim().trim_end_matches('.').trim();
    if !trimmed
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("cast "))
    {
        return None;
    }
    let head = if starts_with_ci(trimmed, "cast ~ only ") {
        trimmed["cast ~ only ".len()..].trim()
    } else if starts_with_ci(trimmed, "cast this spell only ") {
        trimmed["cast this spell only ".len()..].trim()
    } else {
        return None;
    };
    if head.is_empty() {
        return None;
    }

    let parts = split_only_clause_parts(head);
    if parts.is_empty() {
        return None;
    }

    let cp = ctx.interner.checkpoint();
    let mut restrictions = Vec::new();
    let mut conditions = Vec::new();

    for part in parts {
        let mut p = part.trim().to_string();
        if p.to_ascii_lowercase().starts_with("only ") {
            p = p["only ".len()..].trim().to_string();
        }
        let p_ascii_low = p.to_ascii_lowercase();
        let opp_suf_len = if p_ascii_low.ends_with(" on an opponent's turn") {
            Some(" on an opponent's turn".len())
        } else if p_ascii_low.ends_with(" on an opponent’s turn") {
            Some(" on an opponent’s turn".len())
        } else {
            None
        };
        if let Some(suf_len) = opp_suf_len
            && p.len() > suf_len
        {
            push_unique_cast_restriction(
                &mut restrictions,
                SpellCastRestrictionKind::DuringOpponentsTurn,
            );
            let cut = p.len() - suf_len;
            p = p[..cut].trim().to_string();
        } else if p_ascii_low.ends_with(" on your turn") && p.len() > " on your turn".len() {
            push_unique_cast_restriction(
                &mut restrictions,
                SpellCastRestrictionKind::DuringYourTurn,
            );
            let cut = p.len() - " on your turn".len();
            p = p[..cut].trim().to_string();
        }

        let lower = p.replace('’', "'").to_ascii_lowercase();
        match lower.as_str() {
            "during combat" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringCombat,
                );
            }
            "during your turn" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringYourTurn,
                );
            }
            "during an opponent's turn"
            | "during opponents' turns"
            | "during turns other than yours" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringOpponentsTurn,
                );
            }
            "during your upkeep" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringYourUpkeep,
                );
            }
            "before attackers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::BeforeAttackers,
                );
            }
            "during combat before attackers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringCombat,
                );
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::BeforeAttackers,
                );
            }
            "after attackers are declared" | "during the declare attackers step" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::AfterAttackers,
                );
            }
            "during combat after attackers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringCombat,
                );
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::AfterAttackers,
                );
            }
            "before blockers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::BeforeBlockers,
                );
            }
            "during combat before blockers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringCombat,
                );
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::BeforeBlockers,
                );
            }
            "during the declare blockers step" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringBlockers,
                );
            }
            "after blockers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::AfterBlockers,
                );
            }
            "during combat after blockers are declared" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringCombat,
                );
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::AfterBlockers,
                );
            }
            "during your postcombat main phase" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::DuringYourTurn,
                );
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::AfterCombat,
                );
            }
            "during the postcombat main phase"
            | "after combat"
            | "during the end of combat step" => {
                push_unique_cast_restriction(
                    &mut restrictions,
                    SpellCastRestrictionKind::AfterCombat,
                );
            }
            _ => {
                if lower.starts_with("if ") {
                    let cond_slice = p["if ".len()..].trim();
                    if let Some(cond) = parse_restriction_if_condition(cond_slice, ctx) {
                        conditions.push(cond);
                        continue;
                    }
                }
                ctx.interner.rollback(cp);
                return None;
            }
        }
    }

    let cond = match conditions.len() {
        0 => None,
        1 => conditions.into_iter().next(),
        _ => Some(Condition::And { of: conditions }),
    };
    Some((restrictions, cond))
}

fn expand_devotion_clan_shard(s: &str) -> String {
    let mut out = s.to_string();
    for (clan, colors) in [
        ("abzan", "white, black, and green"),
        ("jeskai", "blue, red, and white"),
        ("sultai", "black, green, and blue"),
        ("mardu", "red, white, and black"),
        ("temur", "green, blue, and red"),
        ("bant", "green, white, and blue"),
        ("esper", "white, blue, and black"),
        ("grixis", "blue, black, and red"),
        ("jund", "black, red, and green"),
        ("naya", "red, green, and white"),
    ] {
        let needle = format!("your devotion to {clan}");
        if contains_word_ci(&out, &needle) {
            out = replace_word_ci(&out, &needle, &format!("your devotion to {colors}"));
        }
    }
    out
}

fn parse_activation_cost_reduction_sentence(
    sentence_text: &str,
    ctx: &mut ParseCtx<'_>,
) -> Option<ConditionalCostReduction> {
    let trimmed = sentence_text.trim();
    if !contains_word_ci(trimmed, "this ability costs")
        || !contains_word_ci(trimmed, "less to activate")
    {
        return None;
    }

    let mut s = expand_devotion_clan_shard(trimmed);
    s = replace_word_ci(
        &s,
        "this ability costs x less to activate",
        "This ability costs {X} less to activate",
    );

    // Move trailing "during your turn" to leading "During your turn, ..."
    let s_no_dot = s.trim_end_matches('.').trim();
    if let Some(pos) = find_word_ci_from(s_no_dot, "less to activate during your turn", 0)
        && pos + "less to activate during your turn".len() == s_no_dot.len()
    {
        let head = &s_no_dot[..pos + "less to activate".len()];
        let had_dot = s.trim_end().ends_with('.');
        s = format!("During your turn, {head}{}", if had_dot { "." } else { "" });
    }

    // Extract optional floor / colored-only riders
    let mut body_no_rider = s.trim_end_matches('.').trim().to_string();
    let mut min_total_mana: Option<u16> = None;
    let mut colored_only = false;
    for _ in 0..3 {
        let low = body_no_rider.replace('’', "'").to_ascii_lowercase();
        if let Some(idx) =
            low.rfind(". this effect reduces only the amount of colored mana you pay")
            && idx + ". this effect reduces only the amount of colored mana you pay".len()
                == low.len()
        {
            colored_only = true;
            body_no_rider = body_no_rider[..idx].trim().to_string();
            continue;
        }
        let floor_suffixes = [
            ". this effect can't reduce the amount of mana in that cost to less than one mana",
            ". this effect can't reduce the mana in that cost to less than one mana",
            ". this effect can't reduce the amount of mana that ability costs to activate to less than one mana",
            ". this effect can't reduce the mana that ability costs to activate to less than one mana",
            ". this cost can't be reduced to less than {1}",
        ];
        if let Some(idx) = floor_suffixes
            .iter()
            .find_map(|suf| low.rfind(suf).filter(|&p| p + suf.len() == low.len()))
        {
            min_total_mana = Some(1);
            body_no_rider = body_no_rider[..idx].trim().to_string();
            continue;
        }
        break;
    }

    // Try rewriting to spell cost modifier first
    let mut rewritten = replace_word_ci(&s, "this ability costs", "~ costs");
    rewritten = replace_word_ci(&rewritten, "less to activate", "less to cast");
    rewritten = replace_word_ci(&rewritten, "more to activate", "more to cast");
    if let Some(StaticDef {
        body: StaticBody::CostModifier { delta, .. },
        condition,
    }) = parse_cost_modifier_sentence(&rewritten, ctx)
    {
        return Some(ConditionalCostReduction { delta, condition });
    }

    // Direct fallbacks
    let mut body = body_no_rider.trim();
    let mut turn_cond: Option<Condition> = None;
    if starts_with_ci(body, "during your turn,") {
        turn_cond = Some(Condition::IsTurnOf {
            player: PlayerRef::You,
        });
        body = body["during your turn,".len()..].trim();
    } else if starts_with_ci(body, "during turns other than yours,") {
        turn_cond = Some(Condition::Not {
            of: Box::new(Condition::IsTurnOf {
                player: PlayerRef::You,
            }),
        });
        body = body["during turns other than yours,".len()..].trim();
    }

    let build_delta = |generic: u16, per: Option<Amount>| CostDelta {
        mode: CostDeltaMode::Reduce,
        generic,
        per: per.map(Box::new),
        additional_costs: Vec::new(),
        colored: SmallVec::new(),
        hybrid_colored: SmallVec::new(),
        colored_only,
        min_total_mana,
        max_reduction: None,
        per_target_life: None,
    };

    if starts_with_ci(body, "this ability costs {x} less to activate,") {
        let after_comma = body["this ability costs {x} less to activate,".len()..].trim();
        if starts_with_ci(after_comma, "where x is ") {
            let x_clause = after_comma["where x is ".len()..].trim();
            let x_low = x_clause.replace('’', "'").to_ascii_lowercase();
            let stat = match x_low.as_str() {
                "~'s power" | "its power" => Some(StatKind::Power),
                "~'s toughness" | "its toughness" => Some(StatKind::Toughness),
                "~'s mana value" | "its mana value" => Some(StatKind::ManaValue),
                _ => None,
            };
            if let Some(stat) = stat {
                return Some(ConditionalCostReduction {
                    delta: build_delta(
                        1,
                        Some(Amount::Stat {
                            stat,
                            of: Ref::SelfObj,
                            lki: false,
                        }),
                    ),
                    condition: turn_cond.map(Box::new),
                });
            }
            if starts_with_ci(x_clause, "the number of ") && (x_low.ends_with(" on ~")) {
                let mid = &x_clause["the number of ".len()..x_clause.len() - " on ~".len()].trim();
                let c_word = mid
                    .strip_suffix(" counters")
                    .or_else(|| mid.strip_suffix(" counter"))
                    .or_else(|| mid.strip_suffix(" Counters"))
                    .or_else(|| mid.strip_suffix(" Counter"))
                    .unwrap_or(mid)
                    .trim();
                let ct = if c_word.is_empty() {
                    Some(CounterTarget::Any)
                } else {
                    lookup_counter_id(c_word, ctx.interner).map(CounterTarget::Specific)
                };
                if let Some(counter) = ct {
                    return Some(ConditionalCostReduction {
                        delta: build_delta(
                            1,
                            Some(Amount::CountersOn {
                                counter,
                                of: Ref::SelfObj,
                            }),
                        ),
                        condition: turn_cond.map(Box::new),
                    });
                }
            }
        }
    }

    if starts_with_ci(body, "this ability costs {")
        && let Some(close_brace) = body.find('}')
    {
        let num_s = &body["this ability costs {".len()..close_brace];
        if let Ok(n) = num_s.parse::<u16>() {
            let after = body[close_brace + 1..].trim();
            let after_low = after.to_ascii_lowercase();
            if let Some(each_rest) = after_low.strip_prefix("less to activate for each ")
                && let Some(on_pos) = each_rest.rfind(" on ~")
                && on_pos + " on ~".len() == each_rest.len()
            {
                let mid = each_rest[..on_pos].trim();
                let c_word = mid
                    .strip_suffix("counters")
                    .or_else(|| mid.strip_suffix("counter"))
                    .unwrap_or(mid)
                    .trim();
                let ct = if c_word.is_empty() {
                    Some(CounterTarget::Any)
                } else {
                    lookup_counter_id(c_word, ctx.interner).map(CounterTarget::Specific)
                };
                if let Some(counter) = ct {
                    return Some(ConditionalCostReduction {
                        delta: build_delta(
                            n,
                            Some(Amount::CountersOn {
                                counter,
                                of: Ref::SelfObj,
                            }),
                        ),
                        condition: turn_cond.map(Box::new),
                    });
                }
            }
            if starts_with_ci(after, "less to activate if ") {
                let cond_raw = after["less to activate if ".len()..].trim();
                let synth = format!("Activate only if {cond_raw}.");
                let cond = parse_activated_if_condition_sentence(&synth, ctx)
                    .map(|p| p.condition)
                    .or_else(|| parse_restriction_if_condition(cond_raw, ctx));
                if let Some(c) = cond {
                    let full_cond = match turn_cond {
                        Some(tc) => Condition::And { of: vec![tc, c] },
                        None => c,
                    };
                    return Some(ConditionalCostReduction {
                        delta: build_delta(n, None),
                        condition: Some(Box::new(full_cond)),
                    });
                }
            }
            if turn_cond.is_some() && after_low == "less to activate" {
                return Some(ConditionalCostReduction {
                    delta: build_delta(n, None),
                    condition: turn_cond.map(Box::new),
                });
            }
        }
    }

    None
}

fn blank_spans_in_slice(text: &str, base_offset: u32, spans: &[Span]) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|s| s.start);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for sp in sorted {
        let rel_start = (sp.start.saturating_sub(base_offset) as usize).min(text.len());
        let rel_end = (sp.end.saturating_sub(base_offset) as usize).min(text.len());
        if rel_start > cursor {
            out.push_str(&text[cursor..rel_start]);
            cursor = rel_start;
        }
        if rel_end > cursor {
            for _ in cursor..rel_end {
                out.push(' ');
            }
            cursor = rel_end;
        }
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

fn strip_spans_in_slice(text: &str, base_offset: u32, spans: &[Span]) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let mut sorted = spans.to_vec();
    sorted.sort_by_key(|s| s.start);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for sp in sorted {
        let rel_start = (sp.start.saturating_sub(base_offset) as usize).min(text.len());
        let rel_end = (sp.end.saturating_sub(base_offset) as usize).min(text.len());
        if rel_start > cursor {
            out.push_str(&text[cursor..rel_start]);
            out.push(' ');
        }
        cursor = cursor.max(rel_end);
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

fn extract_activation_restrictions(
    ctx: &mut ParseCtx<'_>,
    effect_text: &str,
    effect_start: u32,
    is_planeswalker: bool,
) -> ActivatedRestrictionsResult {
    let mut restrictions = Vec::new();
    let mut sorcery = is_planeswalker;
    let mut x_cant_be_zero = false;
    let mut activator_who: Option<ActivatorWho> = None;
    let mut conditions: Vec<Condition> = Vec::new();
    let mut explicit_zones: Option<ZoneMask> = None;
    let mut activation_cost_reduction: Option<ConditionalCostReduction> = None;
    let mut stripped_spans: Vec<Span> = Vec::new();
    let mut parse_blank_spans: Vec<Span> = Vec::new();

    let raw_sents = split_sentences(effect_text, effect_start);
    let expanded = crate::normalize::split_quoted_sentence_endings(&raw_sents);
    let mut idx = 0usize;
    while idx < expanded.len() {
        let s = &expanded[idx];
        let s_trim = s.text.trim();
        let s_no_dot = s_trim.trim_end_matches('.').trim();

        // Check activation cost reduction (+ optional trailing rider sentence)
        if activation_cost_reduction.is_none()
            && contains_word_ci(s_trim, "this ability costs")
            && contains_word_ci(s_trim, "less to activate")
        {
            let mut fused_text = s_trim.to_string();
            let mut consumed_next = 0usize;
            if let Some(next_s) = expanded.get(idx + 1) {
                let next_no_dot = next_s.text.trim().trim_end_matches('.').trim();
                let is_rider = next_no_dot.eq_ignore_ascii_case(
                    "this effect reduces only the amount of colored mana you pay",
                ) || starts_with_ci(next_no_dot, "this effect can't reduce the ")
                    || starts_with_ci(next_no_dot, "this effect can’t reduce the ")
                    || next_no_dot
                        .eq_ignore_ascii_case("this cost can't be reduced to less than {1}")
                    || next_no_dot
                        .eq_ignore_ascii_case("this cost can’t be reduced to less than {1}");
                if is_rider {
                    fused_text.push(' ');
                    fused_text.push_str(next_s.text.trim());
                    consumed_next = 1;
                }
            }
            if let Some(red) = parse_activation_cost_reduction_sentence(&fused_text, ctx) {
                activation_cost_reduction = Some(red);
                stripped_spans.push(s.span);
                parse_blank_spans.push(s.span);
                if consumed_next == 1 {
                    let next_span = expanded[idx + 1].span;
                    stripped_spans.push(next_span);
                    parse_blank_spans.push(next_span);
                }
                idx += 1 + consumed_next;
                continue;
            }
        }

        if s_no_dot.eq_ignore_ascii_case("activate only as a sorcery")
            || s_no_dot.eq_ignore_ascii_case("equip only as a sorcery")
        {
            sorcery = true;
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            idx += 1;
            continue;
        }
        if s_no_dot.eq_ignore_ascii_case("x can't be 0")
            || s_no_dot.eq_ignore_ascii_case("x can’t be 0")
        {
            x_cant_be_zero = true;
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            idx += 1;
            continue;
        }
        if s_no_dot.eq_ignore_ascii_case("any player may activate this ability") {
            activator_who = Some(ActivatorWho::Any);
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            idx += 1;
            continue;
        }
        if s_no_dot.eq_ignore_ascii_case("any player may activate this ability only as a sorcery") {
            activator_who = Some(ActivatorWho::Any);
            sorcery = true;
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            idx += 1;
            continue;
        }
        if s_no_dot.eq_ignore_ascii_case("only any opponent may activate this ability")
            || s_no_dot.eq_ignore_ascii_case("only your opponents may activate this ability")
        {
            activator_who = Some(ActivatorWho::Opponent);
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            idx += 1;
            continue;
        }
        if s_no_dot
            .eq_ignore_ascii_case("only any opponent may activate this ability only as a sorcery")
            || s_no_dot.eq_ignore_ascii_case(
                "only your opponents may activate this ability and only as a sorcery",
            )
        {
            activator_who = Some(ActivatorWho::Opponent);
            sorcery = true;
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            idx += 1;
            continue;
        }
        if let Some(cond_parse) = parse_activated_if_condition_sentence(&s.text, ctx) {
            conditions.push(cond_parse.condition);
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
            if let Some(extra) = cond_parse.extra {
                match extra {
                    ActivatedExtraRestriction::AsASorcery => sorcery = true,
                    ActivatedExtraRestriction::OnceEachTurn => {
                        push_unique_activation_restriction(
                            &mut restrictions,
                            ActivationRestriction::OnceEachTurn,
                        );
                    }
                    ActivatedExtraRestriction::DuringYourTurn => {
                        push_unique_activation_restriction(
                            &mut restrictions,
                            ActivationRestriction::DuringYourTurn,
                        );
                    }
                    ActivatedExtraRestriction::DuringYourUpkeep => {
                        push_unique_activation_restriction(
                            &mut restrictions,
                            ActivationRestriction::DuringYourUpkeep,
                        );
                    }
                    ActivatedExtraRestriction::DuringCombat => {
                        push_unique_activation_restriction(
                            &mut restrictions,
                            ActivationRestriction::DuringCombat,
                        );
                    }
                }
            }
            idx += 1;
            continue;
        }
        if let Some((s_sorcery, s_res, s_conds, s_zones)) =
            parse_activate_only_sentence(&s.text, ctx)
        {
            if s_sorcery {
                sorcery = true;
            }
            for r in s_res {
                push_unique_activation_restriction(&mut restrictions, r);
            }
            conditions.extend(s_conds);
            if s_zones.is_some() {
                explicit_zones = s_zones;
            }
            stripped_spans.push(s.span);
            parse_blank_spans.push(s.span);
        }
        idx += 1;
    }

    let activation_condition = match conditions.len() {
        0 => None,
        1 => conditions.into_iter().next(),
        _ => Some(Condition::And { of: conditions }),
    };

    let cleaned_effect_for_parse =
        blank_spans_in_slice(effect_text, effect_start, &parse_blank_spans);
    let cleaned_modal_header = strip_spans_in_slice(effect_text, effect_start, &stripped_spans)
        .trim()
        .to_string();

    ActivatedRestrictionsResult {
        restrictions,
        timing: if sorcery {
            Timing::Sorcery
        } else {
            Timing::Instant
        },
        x_cant_be_zero,
        activator_who,
        activation_condition,
        explicit_zones,
        activation_cost_reduction,
        cleaned_effect_for_parse,
        cleaned_modal_header,
    }
}

fn parse_forecast_cost(cost_text: &str, cost_start: u32, ctx: &mut ParseCtx<'_>) -> CostParse {
    let trimmed = cost_text.trim().trim_end_matches('.').trim();
    let toks = tokenize(trimmed, 0);
    let comma_pos = toks
        .iter()
        .rposition(|t| matches!(t.kind, TokenKind::Punctuation(Punct::Comma)));
    let (head_text, tail_toks) = match comma_pos {
        Some(idx) => {
            let cut = toks[idx].span.start as usize;
            (trimmed[..cut].trim(), &toks[idx + 1..])
        }
        None => ("", toks.as_slice()),
    };
    let tail_matches = match tail_toks {
        [t0, t1, t2, t3, t4] => {
            t0.eq_ci("reveal")
                && matches!(t1.kind, TokenKind::SelfRef)
                && t2.eq_ci("from")
                && t3.eq_ci("your")
                && t4.eq_ci("hand")
        }
        [t0, t1, t2, t3, t4, t5] => {
            t0.eq_ci("reveal")
                && t1.eq_ci("this")
                && t2.is_word()
                && t3.eq_ci("from")
                && t4.eq_ci("your")
                && t5.eq_ci("hand")
        }
        _ => false,
    };
    if !tail_matches {
        return CostParse {
            defs: Vec::new(),
            unparsed: vec![Span {
                start: cost_start,
                end: cost_start + cost_text.len() as u32,
            }],
        };
    }

    let self_reveal = CostDef::RevealFromHand {
        count: Box::new(Amount::Const { n: 1 }),
        target: Some(CostObjectTarget::SelfObj),
        share_color: false,
    };
    if head_text.is_empty() || head_text.eq_ignore_ascii_case("{0}") {
        return CostParse {
            defs: vec![self_reveal],
            unparsed: Vec::new(),
        };
    }
    let mut sub = parse_cost_list(head_text, cost_start, ctx);
    sub.defs.push(self_reveal);
    sub
}

fn parse_non_mana_ability_cost_modifier(body: &str) -> Option<StaticDef> {
    if !body
        .trim_start()
        .get(.."abilities you activate".len())
        .is_some_and(|p| p.eq_ignore_ascii_case("abilities you activate"))
    {
        return None;
    }
    if !starts_with_ci(body.trim(), "abilities you activate ") {
        return None;
    }
    let trimmed = body.trim().trim_end_matches('.').trim().replace('’', "'");
    let low = trimmed.to_ascii_lowercase();
    let prefix = "abilities you activate that aren't mana abilities cost ";
    if !low.starts_with(prefix) {
        return None;
    }
    let rest_orig = &trimmed[prefix.len()..];
    let rest_low = &low[prefix.len()..];
    let (main_part, has_floor) = if let Some(dot_idx) = rest_low.find('.') {
        let tail = rest_low[dot_idx + 1..].trim();
        if tail != "this effect can't reduce the mana in that cost to less than one mana" {
            return None;
        }
        (&rest_orig[..dot_idx], true)
    } else {
        (rest_orig, false)
    };
    let main_low = main_part.trim().to_ascii_lowercase();
    let suffix = " less to activate";
    if !main_low.ends_with(suffix) {
        return None;
    }
    let mana_str = main_part.trim()[..main_part.trim().len() - suffix.len()].trim();
    let inner = mana_str.strip_prefix('{')?.strip_suffix('}')?;
    let generic: u16 = inner.parse().ok()?;
    if generic < 1 {
        return None;
    }
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
                targets_filter: None,
                first_matching_this_turn: false,
            },
            delta: CostDelta {
                mode: CostDeltaMode::Reduce,
                generic,
                per: None,
                additional_costs: Vec::new(),
                colored: SmallVec::new(),
                hybrid_colored: SmallVec::new(),
                colored_only: false,
                min_total_mana: has_floor.then_some(1),
                max_reduction: None,
                per_target_life: None,
            },
            who: PlayerFilter {
                relation: PlayerRelation::You,
            },
            who_enchanted_player: false,
            active_zones: ZoneMask::empty(),
        },
        condition: None,
    })
}

fn infer_activated_zones(
    cost_defs: &[CostDef],
    effect_steps: &[EffectStep],
    explicit_zones: Option<ZoneMask>,
) -> ZoneMask {
    if let Some(z) = explicit_zones {
        return z;
    }
    if cost_defs.iter().any(|d| {
        matches!(
            d,
            CostDef::Discard {
                target: Some(CostObjectTarget::SelfObj),
                ..
            }
        )
    }) {
        return ZoneMask::HAND;
    }
    if cost_defs.iter().any(|d| {
        matches!(
            d,
            CostDef::ExileFrom {
                zone: Zone::Graveyard,
                target: Some(CostObjectTarget::SelfObj),
                ..
            }
        )
    }) || effect_steps.iter().any(|s| {
        matches!(
            s.op,
            EffectOp::Move {
                from: Some(Zone::Graveyard),
                ..
            }
        ) && matches!(s.subject, Selector::SelfObj)
    }) {
        return ZoneMask::GRAVEYARD;
    }
    ZoneMask::BATTLEFIELD
}

fn activated_line(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    wave4_flags: Wave4FaceFlags,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    let line = &group[0];
    let colon = cost_colon(&line.body)?;
    let first_word = line
        .body
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .next()
        .unwrap_or("");
    if first_word.eq_ignore_ascii_case("when")
        || first_word.eq_ignore_ascii_case("whenever")
        || first_word.eq_ignore_ascii_case("at")
        || first_word.eq_ignore_ascii_case("choose")
    {
        return None;
    }

    if let Some(rewrites) = detect_wave4_rewrites(wave4_flags, group) {
        let orig_span = group_span(group);
        let orig_text: String = group
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let saved_mana_tap = ctx.mana_tap_taken;
        let rewritten_group: Vec<NormalizedLine> = group
            .iter()
            .map(|l| rewrite_normalized_line_wave4(l, rewrites))
            .collect();
        if let Some(mut adapted) = activated_line_core(ctx, &rewritten_group, quoted_compiler)
            && adapted
                .iter()
                .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
        {
            for ab in &mut adapted {
                map_ability_wave4(ab, rewrites);
                if let AbilityDef::Activated(act) = ab {
                    act.text = Some(orig_text.clone());
                    act.span = orig_span;
                }
            }
            return Some(adapted);
        }
        ctx.mana_tap_taken = saved_mana_tap;
    }

    activated_line_core_with_colon(ctx, group, colon, quoted_compiler)
}

fn activated_line_core(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    let line = &group[0];
    let colon = cost_colon(&line.body)?;
    let first_word = line
        .body
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .next()
        .unwrap_or("");
    if first_word.eq_ignore_ascii_case("when")
        || first_word.eq_ignore_ascii_case("whenever")
        || first_word.eq_ignore_ascii_case("at")
        || first_word.eq_ignore_ascii_case("choose")
    {
        return None;
    }
    activated_line_core_with_colon(ctx, group, colon, quoted_compiler)
}

fn activated_line_core_with_colon(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    colon: usize,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    let line = &group[0];
    let span = group_span(group);
    let full_text: String = group
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut cost_text = line.body[..colon].trim();
    let mut cost_start = line.body_span.start
        + (line.body[..colon].len() - line.body[..colon].trim_start().len()) as u32;
    let effect_start = line.body_span.start + (colon + 1) as u32;
    let effect_text = &line.body[colon + 1..];

    let mut prefix_abilities: Vec<AbilityDef> = Vec::new();
    let mut cost_keyword = None;
    let mut cost_keyword_name: Option<&str> = None;
    let mut kw_id_override: Option<String> = None;
    let mut forced_zones: Option<ZoneMask> = None;
    let mut power_up_reduction = false;
    let mut extra_restrictions: Vec<ActivationRestriction> = Vec::new();

    for (kw_name, prefix_id) in [
        ("exhaust", "exhaust"),
        ("boast", "boast"),
        ("forecast", "forecast"),
        ("power-up", "power_up"),
    ] {
        if starts_with_ci(cost_text, kw_name) {
            let after_kw = cost_text[kw_name.len()..].trim_start();
            if let Some(after_dash) = after_kw.strip_prefix('—') {
                let inner_cost = after_dash.trim();
                let kw_span = Span {
                    start: line.body_span.start,
                    end: line.body_span.start + colon as u32,
                };
                let kw_id = ctx.interner.intern_keyword(kw_name);
                cost_keyword = Some(kw_id);
                cost_keyword_name = Some(kw_name);
                kw_id_override = Some(format!("{prefix_id}_{}", line.raw_index));
                let param_str = inner_cost.to_string();
                if kw_name == "forecast" {
                    forced_zones = Some(ZoneMask::HAND);
                }
                if kw_name == "power-up" {
                    power_up_reduction = true;
                    extra_restrictions.push(ActivationRestriction::OncePerGame);
                }
                prefix_abilities.push(AbilityDef::Keyword(KeywordAbility {
                    keyword: crate::parser::keyword::get_keyword_spec(kw_name)
                        .map(|_| KeywordDef {
                            keyword: kw_id,
                            cost: None,
                            param: Some(param_str),
                            n: None,
                            cost_defs: Vec::new(),
                            kicker_costs: Vec::new(),
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
                            craft_materials: Vec::new(),
                            craft_any_number: false,
                            craft_share_card_type: false,
                            splice_filter: None,
                            impending_n: None,
                            landwalk_filter: None,
                            suspend_x: false,
                            equip_cost_reduction: None,
                            dynamic_amount: None,
                        })
                        .unwrap(),
                    id: None,
                    span: kw_span,
                }));
                let rel = cost_text.len() - inner_cost.len();
                cost_start += rel as u32;
                cost_text = inner_cost;
                break;
            }
        }
    }

    let cost_span = Span {
        start: cost_start,
        end: line.body_span.start + colon as u32,
    };
    let cost_toks = tokenize(cost_text, cost_start);
    let mut is_loyalty = false;
    let cost = if cost_keyword.is_none()
        && let Some(loy) = parse_loyalty_cost(&cost_toks, cost_text, cost_span, ctx.is_planeswalker)
    {
        is_loyalty = true;
        loy
    } else if cost_keyword_name == Some("forecast") {
        parse_forecast_cost(cost_text, cost_start, ctx)
    } else {
        parse_cost_list(cost_text, cost_start, ctx)
    };
    let cost_complete = cost.unparsed.is_empty();

    let mut x_spend_colors = ColorMask::empty();
    let mut x_colored_only = false;
    let mut x_max_one_per_color = false;
    for d in &cost.defs {
        if let CostDef::Mana {
            x_spend_colors: sc,
            x_colored_only: co,
            x_max_one_per_color: mo,
            ..
        } = d
        {
            x_spend_colors |= *sc;
            x_colored_only |= *co;
            x_max_one_per_color |= *mo;
        }
        if cost_keyword.is_none()
            && let CostDef::Waterbend { cost: wb_cost } = d
        {
            let wb_id = ctx.interner.intern_keyword("waterbend");
            prefix_abilities.push(AbilityDef::Keyword(KeywordAbility {
                keyword: KeywordDef {
                    keyword: wb_id,
                    cost: Some(wb_cost.clone()),
                    param: None,
                    n: None,
                    cost_defs: Vec::new(),
                    kicker_costs: Vec::new(),
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
                    craft_materials: Vec::new(),
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
                span: cost_span,
            }));
        }
    }

    let mut res =
        extract_activation_restrictions(ctx, effect_text, effect_start, ctx.is_planeswalker);
    for r in extra_restrictions {
        push_unique_activation_restriction(&mut res.restrictions, r);
    }

    if group.len() > 1 {
        let id = kw_id_override.unwrap_or_else(|| {
            if is_loyalty {
                format!("loyalty_{}", line.raw_index)
            } else {
                format!("act_{}", line.raw_index)
            }
        });
        let bullets: Vec<ModalBullet<'_>> = group[1..]
            .iter()
            .map(|b| ModalBullet {
                text: &b.text,
                span: b.span,
            })
            .collect();
        let std_modal = if starts_with_ci(res.cleaned_modal_header.trim_start(), "choose") {
            Some(parse_modal_with_quoted(
                &res.cleaned_modal_header,
                &bullets,
                AbilityContext::Activated,
                effect_start,
                None,
                ctx,
                quoted_compiler,
            ))
        } else {
            None
        };
        let std_ok = std_modal.as_ref().is_some_and(|m| {
            m.modal.is_some() && m.unparsed.is_empty() && modal_interpretable(m, false)
        });
        let modal_parse = if std_ok {
            std_modal.unwrap()
        } else {
            parse_expanded_modal_with_quoted(
                &res.cleaned_modal_header,
                &bullets,
                AbilityContext::Activated,
                effect_start,
                None,
                ctx,
                quoted_compiler,
            )
            .or(std_modal)
            .unwrap_or_default()
        };
        let modal_steps: Vec<EffectStep> = modal_parse
            .modal
            .as_ref()
            .map(|m| {
                m.modes
                    .iter()
                    .flat_map(|md| md.effect.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let activation_zones = forced_zones
            .unwrap_or_else(|| infer_activated_zones(&cost.defs, &modal_steps, res.explicit_zones));
        let effect_ok = modal_parse.modal.is_some()
            && modal_parse.unparsed.is_empty()
            && modal_interpretable(&modal_parse, false);
        if !cost_complete || !effect_ok {
            let mut gaps = cost.unparsed;
            if !effect_ok {
                if !modal_parse.unparsed.is_empty() {
                    gaps.extend(modal_parse.unparsed);
                } else {
                    gaps.push(Span {
                        start: effect_start,
                        end: span.end,
                    });
                }
            }
            let ua = UnsupportedAbility {
                reason: UnsupportedReason::Unparsed,
                text: full_text,
                span,
                unparsed: gaps,
                id: Some(id),
                cost: cost.defs,
                cost_complete: Some(cost_complete),
                cost_keyword,
                timing: Some(res.timing),
                is_mana: false,
                effect: Vec::new(),
                targets: Vec::new(),
                activation_restrictions: res.restrictions,
                activation_condition: res.activation_condition.map(Box::new),
                activator_who: res.activator_who,
                power_up_reduction,
                x_cant_be_zero: res.x_cant_be_zero,
                activation_cost_reduction: res.activation_cost_reduction,
                cast_restrictions: Vec::new(),
                cast_condition: None,
                trigger: None,
                condition: None,
                grant_condition: None,
            };
            prefix_abilities.push(AbilityDef::Unsupported(ua));
            return Some(prefix_abilities);
        }
        prefix_abilities.push(AbilityDef::Activated(ActivatedAbility {
            id,
            cost: cost.defs,
            targets: Vec::new(),
            effect: Vec::new(),
            modal: modal_parse.modal,
            timing: res.timing,
            is_mana: false,
            activation_restrictions: res.restrictions,
            activation_condition: res.activation_condition.map(Box::new),
            activator_who: res.activator_who,
            power_up_reduction,
            x_cant_be_zero: res.x_cant_be_zero,
            activation_cost_reduction: res.activation_cost_reduction,
            activation_zones,
            cost_keyword,
            intrinsic: false,
            text: Some(full_text),
            grant_condition: None,
            x_spend_colors,
            x_colored_only,
            x_max_one_per_color,
            span,
        }));
        return Some(prefix_abilities);
    }

    let effect = parse_effects_with_quoted(
        &res.cleaned_effect_for_parse,
        effect_start,
        AbilityContext::Activated,
        None,
        ctx,
        quoted_compiler,
    );
    let is_mana = !is_loyalty
        && cost_keyword.is_none()
        && !effect.steps.is_empty()
        && effect.targets.is_empty()
        && effect
            .steps
            .iter()
            .any(|s| matches!(s.op, EffectOp::AddMana { .. }))
        && effect.steps.iter().all(|s| {
            matches!(s.op, EffectOp::AddMana { .. })
                || (matches!(s.op, EffectOp::FreezeUntap { .. })
                    && matches!(s.subject, Selector::SelfObj)
                    && s.optional.is_none()
                    && s.gate.is_none())
        });
    let timing = if is_mana { Timing::Mana } else { res.timing };
    let activation_zones = forced_zones
        .unwrap_or_else(|| infer_activated_zones(&cost.defs, &effect.steps, res.explicit_zones));

    let id = if is_mana
        && kw_id_override.is_none()
        && cost.defs.as_slice() == [CostDef::TapSelf]
        && !ctx.mana_tap_taken
    {
        ctx.mana_tap_taken = true;
        "mana_tap".to_string()
    } else if let Some(ov) = kw_id_override {
        ov
    } else if is_loyalty {
        format!("loyalty_{}", line.raw_index)
    } else {
        format!("act_{}", line.raw_index)
    };

    let effect_ok = if is_mana {
        effect.unparsed.is_empty() && effect.steps.iter().all(mana_step_complete)
    } else {
        statically_interpretable(&effect, false)
    };

    if !cost_complete || !effect_ok {
        let mut gaps = cost.unparsed;
        if !effect_ok {
            if !effect.unparsed.is_empty() {
                gaps.extend(effect.unparsed);
            } else {
                gaps.push(Span {
                    start: effect_start,
                    end: span.end,
                });
            }
        }
        let mana_steps = if is_mana && effect.steps.iter().all(mana_step_complete) {
            effect.steps
        } else {
            Vec::new()
        };
        let ua = UnsupportedAbility {
            reason: UnsupportedReason::Unparsed,
            text: full_text,
            span,
            unparsed: gaps,
            id: Some(id),
            cost: cost.defs,
            cost_complete: Some(cost_complete),
            cost_keyword,
            timing: Some(timing),
            is_mana,
            effect: mana_steps,
            targets: effect.targets,
            activation_restrictions: res.restrictions,
            activation_condition: res.activation_condition.map(Box::new),
            activator_who: res.activator_who,
            power_up_reduction,
            x_cant_be_zero: res.x_cant_be_zero,
            activation_cost_reduction: res.activation_cost_reduction,
            cast_restrictions: Vec::new(),
            cast_condition: None,
            trigger: None,
            condition: None,
            grant_condition: None,
        };
        prefix_abilities.push(AbilityDef::Unsupported(ua));
        return Some(prefix_abilities);
    }

    prefix_abilities.push(AbilityDef::Activated(ActivatedAbility {
        id,
        cost: cost.defs,
        targets: effect.targets,
        effect: effect.steps,
        modal: None,
        timing,
        is_mana,
        activation_restrictions: res.restrictions,
        activation_condition: res.activation_condition.map(Box::new),
        activator_who: res.activator_who,
        power_up_reduction,
        x_cant_be_zero: res.x_cant_be_zero,
        activation_cost_reduction: res.activation_cost_reduction,
        activation_zones,
        cost_keyword,
        intrinsic: false,
        text: Some(full_text),
        grant_condition: None,
        x_spend_colors,
        x_colored_only,
        x_max_one_per_color,
        span,
    }));
    Some(prefix_abilities)
}

// ---- M2 -> M3 Hook Stubs ---------------------------------------------------------------------

fn extract_meld_partner_from_condition(cond: &Option<Condition>) -> Option<String> {
    fn walk(c: &Condition) -> Option<String> {
        match c {
            Condition::Compare { left, .. } => {
                if let Amount::Count { of } = left.as_ref() {
                    for p in &of.extra {
                        if let ObjectPredicate::Named { name } = p {
                            return Some(name.clone());
                        }
                    }
                }
                None
            }
            Condition::And { of } | Condition::Or { of } => of.iter().find_map(walk),
            Condition::Not { of } => walk(of),
            _ => None,
        }
    }
    cond.as_ref().and_then(walk)
}

fn parse_trigger_meld_effect_with_partner(
    header_effect_text: &str,
    effect_start: u32,
    partner: Option<&str>,
    ctx: &mut ParseCtx<'_>,
) -> Option<EffectParse> {
    let partner_name = partner?;
    let trimmed = header_effect_text.trim();
    let span = Span {
        start: effect_start,
        end: effect_start + header_effect_text.len() as u32,
    };

    let parse_result_and_tapped = |rest: &str| -> Option<(String, bool)> {
        let r = rest.trim();
        let tapped_atk = contains_word_ci(r, "it enters tapped and attacking");
        let before_dot = r.split('.').next().unwrap_or("").trim();
        if before_dot.is_empty() {
            return None;
        }
        Some((before_dot.to_string(), tapped_atk))
    };

    if starts_with_ci(trimmed, "exile them,") {
        let after_exile = trimmed["exile them,".len()..].trim_start();
        if starts_with_ci(after_exile, "then meld them into ") {
            let after_into = &after_exile["then meld them into ".len()..];
            if let Some((res_name, tapped_atk)) = parse_result_and_tapped(after_into) {
                return Some(EffectParse {
                    steps: vec![EffectStep {
                        op: EffectOp::Meld {
                            partner_name: partner_name.to_string(),
                            result_name: res_name,
                            tapped_and_attacking: tapped_atk,
                            partner_must_attack: false,
                        },
                        subject: Selector::SelfObj,
                        actor: None,
                        amount: None,
                        optional: None,
                        gate: None,
                        bind_as: None,
                        do_this_once_per_turn: false,
                        reflexive_trigger: None,
                        span,
                    }],
                    targets: Vec::new(),
                    modal: None,
                    unparsed: Vec::new(),
                });
            }
        }
    }

    if starts_with_ci(trimmed, "you may pay ")
        && let Some(dot_pos) = trimmed.find('.')
    {
        let cost_raw = trimmed["you may pay ".len()..dot_pos].trim();
        let after_dot = trimmed[dot_pos + 1..].trim_start();
        if starts_with_ci(after_dot, "if you do,") {
            let after_do = after_dot["if you do,".len()..].trim_start();
            if starts_with_ci(after_do, "exile them,") {
                let after_exile = after_do["exile them,".len()..].trim_start();
                if starts_with_ci(after_exile, "then meld them into ") {
                    let after_into = &after_exile["then meld them into ".len()..];
                    if let Some((res_name, tapped_atk)) = parse_result_and_tapped(after_into) {
                        let cp = ctx.interner.checkpoint();
                        let cost_parse = parse_cost_list(cost_raw, effect_start, ctx);
                        if cost_parse.unparsed.is_empty()
                            && !cost_parse.defs.is_empty()
                            && cost_parse.defs.iter().all(is_supported_optional_cost)
                        {
                            return Some(EffectParse {
                                steps: vec![EffectStep {
                                    op: EffectOp::Meld {
                                        partner_name: partner_name.to_string(),
                                        result_name: res_name,
                                        tapped_and_attacking: tapped_atk,
                                        partner_must_attack: false,
                                    },
                                    subject: Selector::SelfObj,
                                    actor: None,
                                    amount: None,
                                    optional: Some(OptionalSpec {
                                        chooser: PlayerRef::You,
                                        cost: cost_parse.defs,
                                        mode: OptionalMode::DoIfPaid,
                                    }),
                                    gate: None,
                                    bind_as: None,
                                    do_this_once_per_turn: false,
                                    reflexive_trigger: None,
                                    span,
                                }],
                                targets: Vec::new(),
                                modal: None,
                                unparsed: Vec::new(),
                            });
                        }
                        ctx.interner.rollback(cp);
                    }
                }
            }
        }
    }

    None
}

fn try_split_reflexive_modal(header_effect_text: &str) -> Option<(&str, &str, &str, usize)> {
    let bytes = header_effect_text.as_bytes();
    let mut in_quotes = false;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            in_quotes = !in_quotes;
        } else if !in_quotes && bytes[i] == b'.' {
            let pre = header_effect_text[..=i].trim();
            let rest = header_effect_text[i + 1..].trim_start();
            if starts_with_ci(rest, "when ") {
                let after_when = rest[5..].trim_start();
                if starts_with_ci(after_when, "you ")
                    && let Some(comma) = after_when.find(',')
                {
                    let clause = after_when[..comma].trim();
                    let clause_low = clause.to_ascii_lowercase();
                    let is_refl_clause = clause_low == "you do"
                        || ((clause_low.starts_with("you pay")
                            || clause_low.starts_with("you sacrifice")
                            || clause_low.starts_with("you exile")
                            || clause_low.starts_with("you discard")
                            || clause_low.starts_with("you remove"))
                            && clause_low.ends_with("this way"));
                    if is_refl_clause {
                        let modal_hdr = after_when[comma + 1..].trim_start();
                        if starts_with_ci(modal_hdr, "choose")
                            || starts_with_ci(modal_hdr, "you may choose")
                        {
                            let hdr_offset =
                                modal_hdr.as_ptr() as usize - header_effect_text.as_ptr() as usize;
                            return Some((pre, clause, modal_hdr, hdr_offset));
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn parse_trigger_hook(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    wave4_flags: Wave4FaceFlags,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    let first = &group[0];
    let first_word = first
        .body
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '\'')
        .next()
        .unwrap_or("");
    if !first_word.eq_ignore_ascii_case("when")
        && !first_word.eq_ignore_ascii_case("whenever")
        && !first_word.eq_ignore_ascii_case("at")
    {
        return None;
    }

    if let Some(rewrites) = detect_wave4_rewrites(wave4_flags, group) {
        let orig_span = group_span(group);
        let orig_text: String = group
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let rewritten_group: Vec<NormalizedLine> = group
            .iter()
            .map(|l| rewrite_normalized_line_wave4(l, rewrites))
            .collect();
        if let Some(mut adapted) = trigger_line_core(ctx, &rewritten_group, quoted_compiler)
            && adapted
                .iter()
                .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
        {
            for ab in &mut adapted {
                map_ability_wave4(ab, rewrites);
                if let AbilityDef::Triggered(tr) = ab {
                    tr.text = Some(orig_text.clone());
                    tr.span = orig_span;
                }
            }
            return Some(adapted);
        }
    }

    trigger_line_core(ctx, group, quoted_compiler)
}

fn trigger_line_core(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Option<Vec<AbilityDef>> {
    let line = &group[0];
    let span = group_span(group);
    let full_text: String = group
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let (mut trigger, condition, effect_start) = if let Some(p12) =
        parse_p12_trigger_header(&line.body, line.body_span.start, ctx)
        && let Some(trig) = p12.trigger
    {
        (trig, p12.condition, p12.effect_start)
    } else {
        let head = parse_trigger_header(&line.body, line.body_span.start, ctx);
        let Some(head) = head else {
            return Some(vec![unsupported_ability(
                UnsupportedReason::Unparsed,
                full_text,
                span,
                vec![line.body_span],
            )]);
        };
        let Some(trig) = head.trigger else {
            let gaps = if !head.unparsed.is_empty() {
                head.unparsed
            } else {
                vec![line.body_span]
            };
            return Some(vec![unsupported_ability(
                UnsupportedReason::Unparsed,
                full_text,
                span,
                gaps,
            )]);
        };
        if !head.unparsed.is_empty() {
            return Some(vec![unsupported_ability(
                UnsupportedReason::Unparsed,
                full_text,
                span,
                head.unparsed,
            )]);
        }
        if runtime_trigger_problem(&trig).is_some() {
            let head_span = Span {
                start: line.body_span.start,
                end: head.effect_start,
            };
            return Some(vec![unsupported_ability(
                UnsupportedReason::EventNotImplemented,
                full_text,
                span,
                vec![head_span],
            )]);
        }
        (trig, head.condition, head.effect_start)
    };

    let rel_effect_start =
        (effect_start.saturating_sub(line.span.start) as usize).min(line.text.len());
    let mut header_effect_text = &line.text[rel_effect_start..];
    let header_sentences = split_sentences(header_effect_text, effect_start);
    if header_sentences.len() >= 2 {
        let last = header_sentences.last().unwrap();
        let trimmed = last.text.trim();
        let no_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
        if no_dot.eq_ignore_ascii_case("this ability triggers only once each turn") {
            trigger.only_once_per_turn = true;
            let cut = (last.span.start.saturating_sub(effect_start) as usize)
                .min(header_effect_text.len());
            header_effect_text = header_effect_text[..cut].trim_end();
        }
    }

    let boxed_cond = condition.clone().map(Box::new);

    if group.len() > 1 {
        let bullets: Vec<ModalBullet<'_>> = group[1..]
            .iter()
            .map(|b| ModalBullet {
                text: &b.text,
                span: b.span,
            })
            .collect();
        if let Some((pre_text, refl_clause, modal_hdr, hdr_offset)) =
            try_split_reflexive_modal(header_effect_text)
        {
            let pre_parse = parse_effects_with_quoted(
                pre_text,
                effect_start,
                AbilityContext::Triggered,
                Some(&trigger),
                ctx,
                quoted_compiler,
            );
            let modal_hdr_start = effect_start + hdr_offset as u32;
            let refl_modal = parse_expanded_modal_with_quoted(
                modal_hdr,
                &bullets,
                AbilityContext::Triggered,
                modal_hdr_start,
                Some(&trigger),
                ctx,
                quoted_compiler,
            )
            .or_else(|| {
                Some(parse_modal_with_quoted(
                    modal_hdr,
                    &bullets,
                    AbilityContext::Triggered,
                    modal_hdr_start,
                    Some(&trigger),
                    ctx,
                    quoted_compiler,
                ))
            });
            if pre_parse.unparsed.is_empty()
                && !pre_parse.steps.is_empty()
                && interpretable_trigger(&pre_parse)
                && let Some(ref mp) = refl_modal
                && mp.modal.is_some()
                && mp.unparsed.is_empty()
                && modal_interpretable(mp, true)
            {
                let refl_low = refl_clause.to_ascii_lowercase();
                let ev_filter = if refl_low.contains("remove") && refl_low.contains("counter") {
                    ReflexiveEventFilter::RemovedCounterThisWay
                } else if refl_low.contains("sacrifice") {
                    ReflexiveEventFilter::SacrificedThisWay
                } else if refl_low.contains("exile") {
                    ReflexiveEventFilter::ExiledThisWay
                } else if refl_low.contains("discard") {
                    ReflexiveEventFilter::DiscardedThisWay
                } else {
                    ReflexiveEventFilter::WhenYouDo
                };
                let mut steps = pre_parse.steps;
                if let Some(last) = steps.last_mut() {
                    last.reflexive_trigger = Some(Box::new(ReflexiveTriggerSpec {
                        targets: Vec::new(),
                        effect: Vec::new(),
                        modal: mp.modal.clone().map(Box::new),
                        text: None,
                        event_filter: Some(ev_filter),
                        object_filter: None,
                    }));
                }
                return Some(vec![AbilityDef::Triggered(TriggeredAbility {
                    trigger,
                    condition: boxed_cond,
                    grant_condition: None,
                    targets: pre_parse.targets,
                    effect: steps,
                    modal: None,
                    text: Some(full_text),
                    span,
                })]);
            }
        }

        let std_modal = if starts_with_ci(header_effect_text.trim_start(), "choose") {
            Some(parse_modal_with_quoted(
                header_effect_text,
                &bullets,
                AbilityContext::Triggered,
                effect_start,
                Some(&trigger),
                ctx,
                quoted_compiler,
            ))
        } else {
            None
        };
        let std_ok = std_modal.as_ref().is_some_and(|m| {
            m.modal.is_some() && m.unparsed.is_empty() && modal_interpretable(m, true)
        });
        let modal_parse = if std_ok {
            std_modal.unwrap()
        } else {
            parse_expanded_modal_with_quoted(
                header_effect_text,
                &bullets,
                AbilityContext::Triggered,
                effect_start,
                Some(&trigger),
                ctx,
                quoted_compiler,
            )
            .or(std_modal)
            .unwrap_or_default()
        };
        let modes_ok = modal_parse.modal.is_some()
            && modal_parse.unparsed.is_empty()
            && modal_interpretable(&modal_parse, true);
        if !modes_ok {
            let gaps = if !modal_parse.unparsed.is_empty() {
                modal_parse.unparsed
            } else {
                vec![Span {
                    start: effect_start,
                    end: span.end,
                }]
            };
            let mut ua = unsupported_ability(UnsupportedReason::Unparsed, full_text, span, gaps);
            if let AbilityDef::Unsupported(ref mut u) = ua {
                u.trigger = Some(trigger);
                u.condition = boxed_cond;
            }
            return Some(vec![ua]);
        }
        return Some(vec![AbilityDef::Triggered(TriggeredAbility {
            trigger,
            condition: boxed_cond,
            grant_condition: None,
            targets: Vec::new(),
            effect: Vec::new(),
            modal: modal_parse.modal,
            text: Some(full_text),
            span,
        })]);
    }

    let meld_partner = extract_meld_partner_from_condition(&condition);
    let effect = parse_trigger_meld_effect_with_partner(
        header_effect_text,
        effect_start,
        meld_partner.as_deref(),
        ctx,
    )
    .unwrap_or_else(|| {
        parse_effects_with_quoted_and_cond(
            header_effect_text,
            effect_start,
            AbilityContext::Triggered,
            Some(&trigger),
            condition.as_ref(),
            ctx,
            quoted_compiler,
        )
    });
    if !interpretable_trigger(&effect) {
        let gaps = if !effect.unparsed.is_empty() {
            effect.unparsed
        } else {
            vec![Span {
                start: effect_start,
                end: span.end,
            }]
        };
        let mut ua = unsupported_ability(UnsupportedReason::Unparsed, full_text, span, gaps);
        if let AbilityDef::Unsupported(ref mut u) = ua {
            u.trigger = Some(trigger);
            u.condition = boxed_cond;
        }
        return Some(vec![ua]);
    }

    Some(vec![AbilityDef::Triggered(TriggeredAbility {
        trigger,
        condition: boxed_cond,
        grant_condition: None,
        targets: effect.targets,
        effect: effect.steps,
        modal: effect.modal,
        text: Some(full_text),
        span,
    })])
}

fn parse_loyalty_activations_per_turn(body: &str) -> Option<u8> {
    let prefix = "you may activate the loyalty abilities of ~ ";
    let suffix = " each turn rather than only once";
    let t = body.trim().trim_end_matches('.').trim();
    let suffix_matches = t
        .as_bytes()
        .get(t.len().saturating_sub(suffix.len())..)
        .is_some_and(|s| s.eq_ignore_ascii_case(suffix.as_bytes()));
    if !starts_with_ci(t, prefix) || !suffix_matches {
        return None;
    }
    let mid = t[prefix.len()..t.len() - suffix.len()].trim();
    let mid_low = mid.to_ascii_lowercase();
    if mid_low == "twice" {
        return Some(2);
    }
    if mid_low == "three times" {
        return Some(3);
    }
    if let Some(num_part) = mid_low.strip_suffix(" times")
        && let Ok(n) = num_part.trim().parse::<u8>()
        && n >= 2
    {
        return Some(n);
    }
    None
}

fn parse_static_hook(
    ctx: &mut ParseCtx<'_>,
    group: &[NormalizedLine],
    wave4_flags: Wave4FaceFlags,
) -> Vec<AbilityDef> {
    if let Some(rewrites) = detect_wave4_rewrites(wave4_flags, group) {
        let orig_span = group_span(group);
        let rewritten_group: Vec<NormalizedLine> = group
            .iter()
            .map(|l| rewrite_normalized_line_wave4(l, rewrites))
            .collect();
        let mut adapted = static_group_core(ctx, &rewritten_group);
        if adapted
            .iter()
            .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
        {
            for ab in &mut adapted {
                map_ability_wave4(ab, rewrites);
                match ab {
                    AbilityDef::Static(s) => s.span = orig_span,
                    AbilityDef::Replacement(r) => r.span = orig_span,
                    _ => {}
                }
            }
            return adapted;
        }
    }
    static_group_core(ctx, group)
}

fn static_group_core(ctx: &mut ParseCtx<'_>, group: &[NormalizedLine]) -> Vec<AbilityDef> {
    let span = group_span(group);
    let group_text = || -> String {
        if let [single] = group {
            single.text.clone()
        } else {
            group
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        }
    };

    if group.len() == 1 {
        let line = &group[0];
        let body_trim = line.body.trim();

        if let Some(max) = parse_loyalty_activations_per_turn(body_trim) {
            return vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::LoyaltyActivationsPerTurn { max },
                    condition: None,
                },
                span,
            })];
        }
        let body_no_dot = body_trim.trim_end_matches('.');
        if body_no_dot.eq_ignore_ascii_case("x can't be 0")
            || body_no_dot.eq_ignore_ascii_case("x can’t be 0")
        {
            return vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::SelfCastRestriction {
                        restrictions: Vec::new(),
                        cast_condition: None,
                        x_cant_be_zero: true,
                    },
                    condition: None,
                },
                span,
            })];
        }
        if let Some((restrictions, cast_cond)) =
            parse_spell_cast_restriction_sentence(body_trim, ctx)
        {
            return vec![AbilityDef::Static(StaticAbility {
                static_def: StaticDef {
                    body: StaticBody::SelfCastRestriction {
                        restrictions,
                        cast_condition: cast_cond.map(Box::new),
                        x_cant_be_zero: false,
                    },
                    condition: None,
                },
                span,
            })];
        }
        if let Some(st) = parse_non_mana_ability_cost_modifier(&line.body) {
            return vec![AbilityDef::Static(StaticAbility {
                static_def: st,
                span,
            })];
        }
        if let Some(st) = parse_cost_modifier_sentence(&line.body, ctx) {
            return vec![AbilityDef::Static(StaticAbility {
                static_def: st,
                span,
            })];
        }
        let parsed = parse_statics(&line.body, line.body_span.start, ctx);
        if parsed.unparsed.is_empty() && !parsed.items.is_empty() {
            return parsed
                .items
                .into_iter()
                .map(|item| match item {
                    StaticItem::Static(st) => AbilityDef::Static(StaticAbility {
                        static_def: st,
                        span,
                    }),
                    StaticItem::Replacement(rep) => AbilityDef::Replacement(ReplacementAbility {
                        replacement: rep,
                        span,
                    }),
                })
                .collect();
        }
        if let Some(cp) =
            parse_additional_cost_sentence(&line.body, line.body_span.start, false, ctx)
        {
            if cp.unparsed.is_empty()
                && !cp.defs.is_empty()
                && cp.defs.iter().all(is_cast_payable_cost)
            {
                return vec![AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::AdditionalCastCost { costs: cp.defs },
                        condition: None,
                    },
                    span,
                })];
            }
            let mut ua = unsupported_ability(
                UnsupportedReason::StaticNotCompiled,
                group_text(),
                span,
                vec![span],
            );
            if let AbilityDef::Unsupported(ref mut u) = ua {
                u.id = Some("cast".to_string());
                u.cost = cp.defs;
                u.cost_complete = Some(cp.unparsed.is_empty());
            }
            return vec![ua];
        }
    }

    vec![unsupported_ability(
        UnsupportedReason::StaticNotCompiled,
        group_text(),
        span,
        vec![span],
    )]
}

fn is_cant_be_countered_sentence(s: &str) -> bool {
    let t = s.trim().trim_end_matches('.').trim();
    t.eq_ignore_ascii_case("~ can't be countered")
        || t.eq_ignore_ascii_case("~ can’t be countered")
        || t.eq_ignore_ascii_case("~ cannot be countered")
        || t.eq_ignore_ascii_case("this spell can't be countered")
        || t.eq_ignore_ascii_case("this spell can’t be countered")
        || t.eq_ignore_ascii_case("this spell cannot be countered")
}

fn is_cant_be_copied_sentence(s: &str) -> bool {
    let t = s.trim().trim_end_matches('.').trim();
    t.eq_ignore_ascii_case("~ can't be copied")
        || t.eq_ignore_ascii_case("~ can’t be copied")
        || t.eq_ignore_ascii_case("~ cannot be copied")
        || t.eq_ignore_ascii_case("this spell can't be copied")
        || t.eq_ignore_ascii_case("this spell can’t be copied")
        || t.eq_ignore_ascii_case("this spell cannot be copied")
}

fn blank_spell_lines_spans(lines: &[NormalizedLine], spans: &[Span]) -> Vec<NormalizedLine> {
    if spans.is_empty() {
        return lines.to_vec();
    }
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        let new_text = blank_spans_in_slice(&l.text, l.span.start, spans);
        let new_body = blank_spans_in_slice(&l.body, l.body_span.start, spans);
        if new_body.trim().is_empty() {
            continue;
        }
        let sentences = split_sentences(&new_body, l.body_span.start);
        out.push(NormalizedLine {
            raw_index: l.raw_index,
            raw: l.raw.clone(),
            text: new_text,
            ability_word: l.ability_word.clone(),
            span: l.span,
            body: new_body,
            body_span: l.body_span,
            sentences,
        });
    }
    out
}

struct SpreeSpellState {
    text: String,
    out: Vec<AbilityDef>,
    additional_costs: Vec<CostDef>,
    cast_restrictions: Vec<SpellCastRestrictionKind>,
    cast_condition: Option<Condition>,
    x_cant_be_zero: bool,
    cost_complete: bool,
    unparsed: Vec<Span>,
}

fn compile_spree_or_tiered_spell(
    kind: &str,
    ctx: &mut ParseCtx<'_>,
    lines: &[NormalizedLine],
    span: Span,
    mut state: SpreeSpellState,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Vec<AbilityDef> {
    let mut modes = Vec::new();
    let mut per_mode_costs: Vec<Vec<CostDef>> = Vec::new();
    let mut unparsed = std::mem::take(&mut state.unparsed);
    let mut saw_header = false;

    for line in lines {
        let trimmed = line.text.trim();
        if !saw_header && line.body.trim().eq_ignore_ascii_case(kind) {
            saw_header = true;
            continue;
        }
        if kind == "spree"
            && let Some(after_plus) = trimmed.strip_prefix('+')
            && after_plus.starts_with(char::is_whitespace)
            && let Some(dash) = after_plus.find('—')
        {
            let raw_cost = after_plus[..dash].trim();
            let raw_effect = &after_plus[dash + '—'.len_utf8()..];
            let lead_ws = raw_effect.len() - raw_effect.trim_start().len();
            let eff_trimmed = raw_effect.trim_start();
            let effect_offset = line.text.len() - eff_trimmed.len();
            let _ = lead_ws;
            let effect_start = line.span.start + effect_offset as u32;
            let parsed_cost = parse_cost_list(raw_cost, line.span.start, ctx);
            let cost_ok = parsed_cost.unparsed.is_empty()
                && !parsed_cost.defs.is_empty()
                && parsed_cost.defs.iter().all(|d| {
                    is_cast_payable_cost(d)
                        && !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                });
            if !cost_ok {
                if !parsed_cost.unparsed.is_empty() {
                    unparsed.extend(parsed_cost.unparsed);
                } else {
                    unparsed.push(line.span);
                }
            }
            let eff = parse_effects_with_quoted(
                eff_trimmed,
                effect_start,
                AbilityContext::Spell,
                None,
                ctx,
                quoted_compiler,
            );
            if !eff.unparsed.is_empty() || !statically_interpretable(&eff, false) {
                if !eff.unparsed.is_empty() {
                    unparsed.extend(eff.unparsed);
                } else {
                    unparsed.push(Span {
                        start: effect_start,
                        end: line.span.end,
                    });
                }
            }
            modes.push(mtg_ir::ModalMode {
                targets: eff.targets,
                effect: eff.steps,
                span: line.span,
            });
            per_mode_costs.push(parsed_cost.defs);
            continue;
        }
        if kind == "tiered"
            && let Some(after_bullet) = trimmed.strip_prefix('•')
            && let Some(d1) = after_bullet.find('—')
            && let Some(d2_rel) = after_bullet[d1 + '—'.len_utf8()..].find('—')
        {
            let d2 = d1 + '—'.len_utf8() + d2_rel;
            let raw_cost = after_bullet[d1 + '—'.len_utf8()..d2].trim();
            let raw_effect = after_bullet[d2 + '—'.len_utf8()..].trim_start();
            let effect_offset = line.text.len() - raw_effect.len();
            let effect_start = line.span.start + effect_offset as u32;
            let parsed_cost = if raw_cost == "{0}" {
                CostParse {
                    defs: Vec::new(),
                    unparsed: Vec::new(),
                }
            } else {
                parse_cost_list(raw_cost, line.span.start, ctx)
            };
            let cost_ok = parsed_cost.unparsed.is_empty()
                && (raw_cost == "{0}"
                    || (!parsed_cost.defs.is_empty()
                        && parsed_cost.defs.iter().all(|d| {
                            is_cast_payable_cost(d)
                                && !matches!(d, CostDef::Mana { cost, .. } if cost.x_count > 0 || cost.raw.to_ascii_uppercase().contains("{X}"))
                        })));
            if !cost_ok {
                if !parsed_cost.unparsed.is_empty() {
                    unparsed.extend(parsed_cost.unparsed);
                } else {
                    unparsed.push(line.span);
                }
            }
            let eff = parse_effects_with_quoted(
                raw_effect,
                effect_start,
                AbilityContext::Spell,
                None,
                ctx,
                quoted_compiler,
            );
            if !eff.unparsed.is_empty() || !statically_interpretable(&eff, false) {
                if !eff.unparsed.is_empty() {
                    unparsed.extend(eff.unparsed);
                } else {
                    unparsed.push(Span {
                        start: effect_start,
                        end: line.span.end,
                    });
                }
            }
            modes.push(mtg_ir::ModalMode {
                targets: eff.targets,
                effect: eff.steps,
                span: line.span,
            });
            per_mode_costs.push(parsed_cost.defs);
            continue;
        }
        let all_pre_consumed = line.sentences.iter().all(|s| {
            let trimmed_s = s.text.trim().trim_end_matches('.');
            parse_cost_modifier_sentence(&s.text, ctx).is_some()
                || parse_spell_alt_cast_or_flash_sentence(&s.text, ctx).is_some()
                || is_cant_be_countered_sentence(&s.text)
                || is_cant_be_copied_sentence(&s.text)
                || parse_additional_cost_sentence(&s.text, s.span.start, true, ctx).is_some()
                || parse_spell_cast_restriction_sentence(&s.text, ctx).is_some()
                || trimmed_s.eq_ignore_ascii_case("x can't be 0")
                || trimmed_s.eq_ignore_ascii_case("x can’t be 0")
        });
        if !all_pre_consumed {
            unparsed.push(line.span);
        }
    }

    if !saw_header || modes.is_empty() || !unparsed.is_empty() {
        let gaps = if !unparsed.is_empty() {
            unparsed
        } else {
            vec![span]
        };
        let mut ua = unsupported_ability(UnsupportedReason::Unparsed, state.text, span, gaps);
        if let AbilityDef::Unsupported(ref mut u) = ua {
            u.id = Some("cast".to_string());
            u.cost = state.additional_costs;
            u.cost_complete = Some(state.cost_complete);
            u.cast_restrictions = state.cast_restrictions;
            u.cast_condition = state.cast_condition.map(Box::new);
            u.x_cant_be_zero = state.x_cant_be_zero;
        }
        state.out.push(ua);
        return state.out;
    }

    let max = if kind == "spree" {
        modes.len() as u16
    } else {
        1
    };
    let modal = ModalDef {
        min: 1,
        max,
        allow_repeat: false,
        once_per_turn_unique: false,
        unchosen_permanent: false,
        up_to: false,
        conditional_choose_all: None,
        conditional_choose_range: None,
        conditional_not_random: None,
        different_player_targets_per_mode: false,
        opponent_chooses: None,
        random_mode: false,
        dynamic_max: None,
        pawprint_budget: None,
        pawprint_costs: Vec::new(),
        prefix_effect: Vec::new(),
        suffix_effect: Vec::new(),
        modes,
        per_mode_costs,
    };
    state.out.push(AbilityDef::Spell(SpellAbility {
        id: None,
        additional_costs: state.additional_costs,
        targets: Vec::new(),
        effect: Vec::new(),
        modal: Some(modal),
        cast_restrictions: state.cast_restrictions,
        cast_condition: state.cast_condition.map(Box::new),
        x_cant_be_zero: state.x_cant_be_zero,
        span,
    }));
    state.out
}

fn parse_spell_hook(
    ctx: &mut ParseCtx<'_>,
    norm: &NormalizedFace,
    spell_lines: &[NormalizedLine],
    wave4_flags: Wave4FaceFlags,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Vec<AbilityDef> {
    if let Some(rewrites) = detect_wave4_rewrites(wave4_flags, spell_lines) {
        let orig_span = Span {
            start: spell_lines[0].span.start,
            end: spell_lines.last().unwrap().span.end,
        };
        let rewritten_text = apply_wave4_text_rewrite(&norm.text, rewrites);
        let rewritten_norm_lines: Vec<NormalizedLine> = norm
            .lines
            .iter()
            .map(|l| rewrite_normalized_line_wave4(l, rewrites))
            .collect();
        let rewritten_norm = NormalizedFace {
            text: rewritten_text,
            lines: rewritten_norm_lines,
        };
        let rewritten_spell_lines: Vec<NormalizedLine> = spell_lines
            .iter()
            .map(|l| rewrite_normalized_line_wave4(l, rewrites))
            .collect();
        let mut adapted = spell_ability_core(
            ctx,
            &rewritten_norm,
            &rewritten_spell_lines,
            quoted_compiler,
        );
        if adapted
            .iter()
            .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
        {
            for ab in &mut adapted {
                map_ability_wave4(ab, rewrites);
                if let AbilityDef::Spell(sp) = ab {
                    sp.span = orig_span;
                }
            }
            return adapted;
        }
    }
    spell_ability_core(ctx, norm, spell_lines, quoted_compiler)
}

fn spell_ability_core(
    ctx: &mut ParseCtx<'_>,
    norm: &NormalizedFace,
    spell_lines: &[NormalizedLine],
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> Vec<AbilityDef> {
    let first_span = spell_lines[0].span;
    let last_span = spell_lines.last().unwrap().span;
    let total_span = Span {
        start: first_span.start,
        end: last_span.end,
    };
    let full_text: String = spell_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut out = Vec::new();
    let mut additional_costs: Vec<CostDef> = Vec::new();
    let mut cast_restrictions: Vec<SpellCastRestrictionKind> = Vec::new();
    let mut cast_conditions: Vec<Condition> = Vec::new();
    let mut cost_complete = true;
    let mut x_cant_be_zero = false;
    let mut unparsed: Vec<Span> = Vec::new();
    let mut pre_consumed_spans: Vec<Span> = Vec::new();

    for line in spell_lines {
        for s in &line.sentences {
            if let Some(st) = parse_cost_modifier_sentence(&s.text, ctx) {
                out.push(AbilityDef::Static(StaticAbility {
                    static_def: st,
                    span: s.span,
                }));
                pre_consumed_spans.push(s.span);
                continue;
            }
            if let Some(st) = parse_spell_alt_cast_or_flash_sentence(&s.text, ctx) {
                out.push(AbilityDef::Static(StaticAbility {
                    static_def: st,
                    span: s.span,
                }));
                pre_consumed_spans.push(s.span);
                continue;
            }
            if is_cant_be_countered_sentence(&s.text) {
                out.push(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Restriction {
                            affected: Affected::SelfObj,
                            restriction: RestrictionKind::CantBeCountered,
                        },
                        condition: None,
                    },
                    span: s.span,
                }));
                pre_consumed_spans.push(s.span);
                continue;
            }
            if is_cant_be_copied_sentence(&s.text) {
                out.push(AbilityDef::Static(StaticAbility {
                    static_def: StaticDef {
                        body: StaticBody::Restriction {
                            affected: Affected::SelfObj,
                            restriction: RestrictionKind::CantBeCopied,
                        },
                        condition: None,
                    },
                    span: s.span,
                }));
                pre_consumed_spans.push(s.span);
                continue;
            }
            if starts_with_ci(
                s.text.trim(),
                "if ~ would be put into a graveyard from anywhere",
            ) {
                let parsed = parse_statics(&s.text, s.span.start, ctx);
                if parsed.unparsed.is_empty() && !parsed.items.is_empty() {
                    for item in parsed.items {
                        match item {
                            StaticItem::Static(st) => out.push(AbilityDef::Static(StaticAbility {
                                static_def: st,
                                span: s.span,
                            })),
                            StaticItem::Replacement(rep) => {
                                out.push(AbilityDef::Replacement(ReplacementAbility {
                                    replacement: rep,
                                    span: s.span,
                                }))
                            }
                        }
                    }
                    pre_consumed_spans.push(s.span);
                    continue;
                }
            }
            if let Some(cp) = parse_additional_cost_sentence(&s.text, s.span.start, true, ctx) {
                additional_costs.extend(cp.defs);
                if !cp.unparsed.is_empty() {
                    cost_complete = false;
                    unparsed.extend(cp.unparsed);
                }
                pre_consumed_spans.push(s.span);
                continue;
            }
            let trimmed_s = s.text.trim().trim_end_matches('.');
            if trimmed_s.eq_ignore_ascii_case("x can't be 0")
                || trimmed_s.eq_ignore_ascii_case("x can’t be 0")
            {
                x_cant_be_zero = true;
                pre_consumed_spans.push(s.span);
                continue;
            }
            if let Some((rs, cond)) = parse_spell_cast_restriction_sentence(&s.text, ctx) {
                for r in rs {
                    push_unique_cast_restriction(&mut cast_restrictions, r);
                }
                if let Some(c) = cond {
                    cast_conditions.push(c);
                }
                pre_consumed_spans.push(s.span);
                continue;
            }
            if (trimmed_s.eq_ignore_ascii_case("you may cast ~ as though it had flash")
                || trimmed_s.eq_ignore_ascii_case("you may cast this spell as though it had flash"))
                && let Some(st) = parse_static_spell_restriction_or_cast_mod(&s.text, ctx)
            {
                out.push(AbilityDef::Static(StaticAbility {
                    static_def: st,
                    span: s.span,
                }));
                pre_consumed_spans.push(s.span);
            }
        }
    }

    let cast_condition = match cast_conditions.len() {
        0 => None,
        1 => cast_conditions.into_iter().next(),
        _ => Some(Condition::And {
            of: cast_conditions,
        }),
    };

    if spell_lines.iter().any(is_spree_line) {
        return compile_spree_or_tiered_spell(
            "spree",
            ctx,
            spell_lines,
            total_span,
            SpreeSpellState {
                text: full_text,
                out,
                additional_costs,
                cast_restrictions,
                cast_condition,
                x_cant_be_zero,
                cost_complete,
                unparsed,
            },
            quoted_compiler,
        );
    }
    if spell_lines.iter().any(is_tiered_line) {
        return compile_spree_or_tiered_spell(
            "tiered",
            ctx,
            spell_lines,
            total_span,
            SpreeSpellState {
                text: full_text,
                out,
                additional_costs,
                cast_restrictions,
                cast_condition,
                x_cant_be_zero,
                cost_complete,
                unparsed,
            },
            quoted_compiler,
        );
    }

    let filtered_lines = blank_spell_lines_spans(spell_lines, &pre_consumed_spans);
    let mut effect = parse_spell_face(&filtered_lines, ctx, quoted_compiler);
    let mut spell_interpretable = if effect.modal.is_some() {
        modal_interpretable(&effect, false)
    } else {
        statically_interpretable(&effect, false)
    };

    if (!effect.unparsed.is_empty()
        || !spell_interpretable
        || (effect.modal.is_some() && !effect.steps.is_empty()))
        && filtered_lines.len() > 1
        && let Some(bullet_idx) = filtered_lines
            .iter()
            .position(|l| l.text.trim_start().starts_with('•'))
        && bullet_idx > 0
    {
        let header_line = &filtered_lines[bullet_idx - 1];
        let mut end_idx = bullet_idx;
        while end_idx < filtered_lines.len()
            && filtered_lines[end_idx].text.trim_start().starts_with('•')
        {
            end_idx += 1;
        }
        let bullets: Vec<ModalBullet<'_>> = filtered_lines[bullet_idx..end_idx]
            .iter()
            .map(|l| ModalBullet {
                text: &l.text,
                span: l.span,
            })
            .collect();
        if let Some(fb) = parse_expanded_modal_with_quoted(
            &header_line.text,
            &bullets,
            AbilityContext::Spell,
            header_line.span.start,
            None,
            ctx,
            quoted_compiler,
        ) && fb.modal.is_some()
        {
            let fb_ok = fb.unparsed.is_empty() && modal_interpretable(&fb, false);
            if fb_ok
                || effect.modal.is_none()
                || !effect.steps.is_empty()
                || fb.unparsed.len() < effect.unparsed.len()
            {
                effect = fb;
                spell_interpretable = fb_ok;
            }
        }
    }

    let _ = norm;
    unparsed.extend(effect.unparsed);
    if !unparsed.is_empty() || !spell_interpretable {
        let gaps = if !unparsed.is_empty() {
            unparsed
        } else {
            vec![total_span]
        };
        let fallback_targets = if !effect.targets.is_empty() {
            effect.targets
        } else {
            extract_unparsed_targets(&full_text, ctx)
        };
        let mut ua = unsupported_ability(UnsupportedReason::Unparsed, full_text, total_span, gaps);
        if let AbilityDef::Unsupported(ref mut u) = ua {
            u.id = Some("cast".to_string());
            u.cost = additional_costs;
            u.cost_complete = Some(cost_complete);
            u.targets = fallback_targets;
            u.cast_restrictions = cast_restrictions;
            u.cast_condition = cast_condition.map(Box::new);
            u.x_cant_be_zero = x_cant_be_zero;
        }
        out.push(ua);
        return out;
    }

    let (targets, steps, modal) = if let Some(m) = effect.modal {
        (Vec::new(), Vec::new(), Some(m))
    } else {
        (effect.targets, effect.steps, None)
    };
    out.push(AbilityDef::Spell(SpellAbility {
        id: None,
        additional_costs,
        targets,
        effect: steps,
        modal,
        cast_restrictions,
        cast_condition: cast_condition.map(Box::new),
        x_cant_be_zero,
        span: total_span,
    }));
    out
}

fn is_spree_line(l: &NormalizedLine) -> bool {
    l.body.trim().eq_ignore_ascii_case("spree")
}

fn is_tiered_line(l: &NormalizedLine) -> bool {
    l.body.trim().eq_ignore_ascii_case("tiered")
}

fn strip_cleave_bracket_delimiters(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch != '[' && ch != ']' {
            out.push(ch);
        }
    }
    out
}

fn strip_cleave_bracketed_segments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for ch in s.chars() {
        if ch == '[' {
            depth += 1;
        } else if ch == ']' {
            depth = depth.saturating_sub(1);
        } else if depth == 0 {
            out.push(ch);
        }
    }
    // Collapse double spaces left around stripped segments and remove space before punctuation
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for ch in out.chars() {
        if ch == ' ' {
            if !prev_space && !collapsed.is_empty() {
                collapsed.push(' ');
                prev_space = true;
            }
        } else {
            if prev_space && matches!(ch, '.' | ',' | ';' | ':') {
                collapsed.pop();
            }
            collapsed.push(ch);
            prev_space = false;
        }
    }
    collapsed.trim().to_string()
}

fn rebuild_cleave_subset(
    transform: fn(&str) -> String,
    spell_lines: &[NormalizedLine],
) -> (NormalizedFace, Vec<NormalizedLine>) {
    let mut rebuilt_lines = Vec::with_capacity(spell_lines.len());
    let mut parts = Vec::with_capacity(spell_lines.len());
    let mut offset = 0u32;
    for (idx, l) in spell_lines.iter().enumerate() {
        if idx > 0 {
            offset += 1;
        }
        let new_text = transform(&l.text);
        let new_body = transform(&l.body);
        let span = Span {
            start: offset,
            end: offset + new_text.len() as u32,
        };
        let body_rel = l
            .body_span
            .start
            .saturating_sub(l.span.start)
            .min(new_text.len() as u32);
        let body_span = Span {
            start: offset + body_rel,
            end: offset + new_text.len() as u32,
        };
        let sentences = split_sentences(&new_body, body_span.start);
        rebuilt_lines.push(NormalizedLine {
            raw_index: l.raw_index,
            raw: l.raw.clone(),
            text: new_text.clone(),
            ability_word: l.ability_word.clone(),
            span,
            body: new_body,
            body_span,
            sentences,
        });
        parts.push(new_text);
        offset = span.end;
    }
    let norm = NormalizedFace {
        text: parts.join("\n"),
        lines: rebuilt_lines.clone(),
    };
    (norm, rebuilt_lines)
}

fn compile_cleave_spell_bundle(
    ctx: &mut ParseCtx<'_>,
    norm: &NormalizedFace,
    spell_lines: &[NormalizedLine],
    wave4_flags: Wave4FaceFlags,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> (Vec<AbilityDef>, Option<SpellAbility>, bool) {
    let orig_span = Span {
        start: spell_lines[0].span.start,
        end: spell_lines.last().unwrap().span.end,
    };
    let has_brackets = spell_lines
        .iter()
        .any(|l| l.text.contains('[') && l.text.contains(']'));
    if !has_brackets {
        let base = parse_spell_hook(ctx, norm, spell_lines, wave4_flags, quoted_compiler);
        let base_spell = base.iter().find_map(|a| {
            if let AbilityDef::Spell(sp) = a {
                Some(sp.clone())
            } else {
                None
            }
        });
        let ok = base_spell.is_some()
            && base
                .iter()
                .all(|a| !matches!(a, AbilityDef::Unsupported(_)));
        return (base, base_spell, ok);
    }

    let (uncleaved_norm, uncleaved_lines) =
        rebuild_cleave_subset(strip_cleave_bracket_delimiters, spell_lines);
    let mut uncleaved_out = if !uncleaved_lines.is_empty() {
        parse_spell_hook(
            ctx,
            &uncleaved_norm,
            &uncleaved_lines,
            wave4_flags,
            quoted_compiler,
        )
    } else {
        vec![unsupported_ability(
            UnsupportedReason::Unparsed,
            norm.text
                .get(orig_span.start as usize..orig_span.end as usize)
                .unwrap_or("")
                .to_string(),
            orig_span,
            vec![orig_span],
        )]
    };
    for ab in &mut uncleaved_out {
        match ab {
            AbilityDef::Spell(sp) => sp.span = orig_span,
            AbilityDef::Unsupported(ua) => ua.span = orig_span,
            _ => {}
        }
    }

    let (cleaved_norm, cleaved_lines) =
        rebuild_cleave_subset(strip_cleave_bracketed_segments, spell_lines);
    let cleaved_out = if !cleaved_lines.is_empty() {
        parse_spell_hook(
            ctx,
            &cleaved_norm,
            &cleaved_lines,
            wave4_flags,
            quoted_compiler,
        )
    } else {
        Vec::new()
    };
    let cleaved_spell = cleaved_out.iter().find_map(|a| {
        if let AbilityDef::Spell(sp) = a {
            let mut sp2 = sp.clone();
            sp2.span = orig_span;
            Some(sp2)
        } else {
            None
        }
    });
    let cleave_ok = uncleaved_out
        .iter()
        .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
        && !cleaved_out.is_empty()
        && cleaved_out
            .iter()
            .all(|a| !matches!(a, AbilityDef::Unsupported(_)))
        && cleaved_spell.is_some();

    (uncleaved_out, cleaved_spell, cleave_ok)
}

fn compile_gift_spell_bundle(
    ctx: &mut ParseCtx<'_>,
    norm: &NormalizedFace,
    spell_lines: &[NormalizedLine],
    wave4_flags: Wave4FaceFlags,
    quoted_compiler: &dyn QuotedAbilityCompiler,
) -> (Vec<AbilityDef>, Option<SpellAbility>) {
    // CR 702.174b replacement-style Gift spell: "<Base>. If (the|~'s) gift was promised, <Promised> instead."
    let orig_span = Span {
        start: spell_lines[0].span.start,
        end: spell_lines.last().unwrap().span.end,
    };
    let joined: String = spell_lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let joined_trim = joined.trim();
    let split_marker = [". if the gift was promised,", ". if ~'s gift was promised,"]
        .iter()
        .find_map(|m| find_word_ci_from(joined_trim, &m[2..], 0).map(|idx| (idx, m.len() - 2)));
    if let Some((marker_idx, marker_len)) = split_marker {
        let before_marker = joined_trim[..marker_idx].trim_end();
        let after_marker = joined_trim[marker_idx + marker_len..].trim();
        let after_no_dot = after_marker.trim_end_matches('.').trim();
        let instead_len = " instead".len();
        if before_marker.ends_with('.')
            && after_no_dot.len() >= instead_len
            && after_no_dot.is_char_boundary(after_no_dot.len() - instead_len)
            && after_no_dot[after_no_dot.len() - instead_len..].eq_ignore_ascii_case(" instead")
        {
            let promised_body = after_no_dot[..after_no_dot.len() - instead_len].trim();
            if !promised_body.is_empty() {
                let cp = ctx.interner.checkpoint();
                let unpromised_text = before_marker.to_string();
                let mut promised_chars = promised_body.chars();
                let first_ch = promised_chars.next().unwrap();
                let promised_text = format!(
                    "{}{}.",
                    first_ch.to_ascii_uppercase(),
                    promised_chars.as_str()
                );
                let unpromised_norm = normalize_face("~", &unpromised_text, false);
                let promised_norm = normalize_face("~", &promised_text, false);
                if !unpromised_norm.lines.is_empty() && !promised_norm.lines.is_empty() {
                    let mut unpromised_abs = parse_spell_hook(
                        ctx,
                        &unpromised_norm,
                        &unpromised_norm.lines,
                        wave4_flags,
                        quoted_compiler,
                    );
                    for ab in &mut unpromised_abs {
                        match ab {
                            AbilityDef::Spell(sp) => sp.span = orig_span,
                            AbilityDef::Unsupported(ua) => ua.span = orig_span,
                            _ => {}
                        }
                    }
                    let promised_abs = parse_spell_hook(
                        ctx,
                        &promised_norm,
                        &promised_norm.lines,
                        wave4_flags,
                        quoted_compiler,
                    );
                    let promised_spell = promised_abs.iter().find_map(|a| {
                        if let AbilityDef::Spell(sp) = a {
                            let mut sp2 = sp.clone();
                            sp2.span = orig_span;
                            Some(sp2)
                        } else {
                            None
                        }
                    });
                    if !unpromised_abs
                        .iter()
                        .any(|a| matches!(a, AbilityDef::Unsupported(_)))
                        && !promised_abs
                            .iter()
                            .any(|a| matches!(a, AbilityDef::Unsupported(_)))
                        && let Some(ps) = promised_spell
                    {
                        return (unpromised_abs, Some(ps));
                    }
                }
                ctx.interner.rollback(cp);
            }
        }
    }

    let direct = parse_spell_hook(ctx, norm, spell_lines, wave4_flags, quoted_compiler);
    (direct, None)
}
