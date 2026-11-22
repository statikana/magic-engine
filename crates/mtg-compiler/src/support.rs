use std::collections::HashSet;

use mtg_ir::{AbilityDef, CardDef, CardTypeMask, Span, UnsupportedAbility, UnsupportedReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnsupportedAbilityKind {
    Spell,
    Activated,
    Mana,
    Triggered,
    TriggerUnrecognised,
    Static,
    Keyword,
    Multiface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedAbilityReport {
    pub face: String,
    pub kind: UnsupportedAbilityKind,
    pub text: String,
    pub reason: UnsupportedReason,
    pub span: Span,
    pub unparsed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardSupportReport {
    pub fully_supported: bool,
    pub unsupported: Vec<UnsupportedAbilityReport>,
}

pub fn unsupported_kind_label(kind: UnsupportedAbilityKind) -> &'static str {
    match kind {
        UnsupportedAbilityKind::Spell => "Spell",
        UnsupportedAbilityKind::Activated => "Activated",
        UnsupportedAbilityKind::Mana => "Mana",
        UnsupportedAbilityKind::Triggered => "Triggered",
        UnsupportedAbilityKind::TriggerUnrecognised => "Trigger (unrecognised)",
        UnsupportedAbilityKind::Static => "Static",
        UnsupportedAbilityKind::Keyword => "Keyword",
        UnsupportedAbilityKind::Multiface => "Multiface",
    }
}

fn starts_with_trigger_word(text: &str) -> bool {
    let trimmed = text.trim_start();
    for word in ["when", "whenever", "at"] {
        if trimmed.len() >= word.len()
            && trimmed.is_char_boundary(word.len())
            && trimmed[..word.len()].eq_ignore_ascii_case(word)
        {
            let next = trimmed[word.len()..].chars().next();
            if next.is_none_or(|c| !c.is_alphanumeric() && c != '_') {
                return true;
            }
        }
    }
    false
}

fn has_mana_effect(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while let Some(rel) = text[i..].find(':') {
        let colon = i + rel;
        let after = text[colon + 1..].trim_start();
        if after.len() >= 3 && after.is_char_boundary(3) && after[..3].eq_ignore_ascii_case("add") {
            let next = after[3..].chars().next();
            if next.is_none_or(|c| !c.is_alphanumeric() && c != '_') {
                return true;
            }
        }
        i = colon + 1;
        if i >= bytes.len() {
            break;
        }
    }
    false
}

/// Zero-regex detector for activated-ability lines (`cost: effect`).
pub fn looks_like_activated_line(text: &str) -> bool {
    let mut colon_idx: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if ch == '"' || ch == '“' {
            return false;
        }
        if ch == ':' {
            colon_idx = Some(idx);
            break;
        }
    }
    let Some(colon) = colon_idx else {
        return false;
    };
    let after = &text[colon + 1..];
    if after.trim_start().is_empty() {
        return false;
    }
    let before = &text[..colon];
    if before.contains('{') && before.contains('}') {
        return true;
    }
    // Check [+−-](\d+|X)
    let mut pos = 0usize;
    while pos < before.len() {
        let ch = before[pos..].chars().next().unwrap();
        if matches!(ch, '+' | '-' | '−') {
            let rest = &before[pos + ch.len_utf8()..];
            if rest.starts_with('X')
                || rest.starts_with('x')
                || rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                return true;
            }
        }
        let prev_is_word = before[..pos]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !prev_is_word {
            let rest = &before[pos..];
            for kw in [
                "tap",
                "untap",
                "sacrifice",
                "discard",
                "exile",
                "pay",
                "remove",
            ] {
                if rest.len() >= kw.len()
                    && rest.is_char_boundary(kw.len())
                    && rest[..kw.len()].eq_ignore_ascii_case(kw)
                    && rest[kw.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_alphanumeric() && c != '_')
                {
                    return true;
                }
            }
        }
        pos += ch.len_utf8();
    }
    false
}

fn unsupported_kind(ab: &UnsupportedAbility, is_spell: bool) -> UnsupportedAbilityKind {
    match ab.reason {
        UnsupportedReason::StaticNotCompiled => UnsupportedAbilityKind::Static,
        UnsupportedReason::KeywordNotImplemented => UnsupportedAbilityKind::Keyword,
        UnsupportedReason::Multiface => UnsupportedAbilityKind::Multiface,
        UnsupportedReason::EventNotImplemented => UnsupportedAbilityKind::TriggerUnrecognised,
        UnsupportedReason::Unparsed => {
            let text = ab.text.trim();
            if ab.trigger.is_some() {
                return UnsupportedAbilityKind::Triggered;
            }
            if starts_with_trigger_word(text) {
                return UnsupportedAbilityKind::TriggerUnrecognised;
            }
            if is_spell && (ab.id.is_none() || ab.id.as_deref() == Some("cast")) {
                return UnsupportedAbilityKind::Spell;
            }
            if ab.id.is_some() || !ab.cost.is_empty() || looks_like_activated_line(text) {
                if ab.is_mana || has_mana_effect(text) {
                    return UnsupportedAbilityKind::Mana;
                }
                return UnsupportedAbilityKind::Activated;
            }
            if is_spell {
                UnsupportedAbilityKind::Spell
            } else {
                UnsupportedAbilityKind::Static
            }
        }
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

pub fn extract_unparsed_fragments(ab: &UnsupportedAbility) -> Vec<String> {
    let full_trimmed = ab.text.trim();
    let mut out = Vec::new();
    for u in &ab.unparsed {
        let rel_start =
            floor_char_boundary(&ab.text, u.start.saturating_sub(ab.span.start) as usize);
        let rel_end = floor_char_boundary(&ab.text, u.end.saturating_sub(ab.span.start) as usize);
        if rel_start < rel_end
            && let Some(slice) = ab.text.get(rel_start..rel_end)
        {
            let frag = slice.trim();
            if !frag.is_empty() && frag != full_trimmed {
                out.push(frag.to_string());
            }
        }
    }
    out
}

pub fn definition_support_report(def: &CardDef) -> CardSupportReport {
    let mut unsupported = Vec::new();
    for face in &def.faces {
        let is_spell = face
            .characteristics
            .types
            .intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY);
        for ab in &face.abilities {
            if let AbilityDef::Unsupported(ua) = ab {
                unsupported.push(UnsupportedAbilityReport {
                    face: face.name.clone(),
                    kind: unsupported_kind(ua, is_spell),
                    text: ua.text.clone(),
                    reason: ua.reason,
                    span: ua.span,
                    unparsed: extract_unparsed_fragments(ua),
                });
            }
        }
    }
    CardSupportReport {
        fully_supported: unsupported.is_empty(),
        unsupported,
    }
}

/// Exact 15 number words from TS `scripts/magic-shared.ts:33`.
const TEMPLATE_NUMBER_WORDS: [&str; 15] = [
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
    "twelve", "thirteen", "fifteen", "twenty",
];

#[inline]
fn is_js_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Template cluster normalizer matching TS `scripts/magic-shared.ts:32-49` without regexes.
#[derive(Debug, Clone)]
pub struct Templater {
    subtype_words: HashSet<String>,
}

pub fn build_templater<I, S>(subtypes: I) -> Templater
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut subtype_words = HashSet::new();
    for item in subtypes {
        let s = item.as_ref();
        if s.is_empty() {
            continue;
        }
        subtype_words.insert(s.to_string());
        match s {
            "Elf" => {
                subtype_words.insert("Elves".to_string());
            }
            "Dwarf" => {
                subtype_words.insert("Dwarves".to_string());
            }
            "Wolf" => {
                subtype_words.insert("Wolves".to_string());
            }
            _ => {
                subtype_words.insert(format!("{s}s"));
            }
        }
        if s.ends_with('s') || s.ends_with('x') || s.ends_with("ch") || s.ends_with("sh") {
            subtype_words.insert(format!("{s}es"));
        }
    }
    Templater { subtype_words }
}

impl Templater {
    pub fn apply(&self, text: &str) -> String {
        let pass1 = replace_braced_costs(text);
        let pass2 = replace_numbers(&pass1);
        let pass3 = replace_number_words(&pass2);
        self.replace_subtypes(&pass3)
    }

    fn replace_subtypes(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut pos = 0usize;
        while pos < text.len() {
            let ch = text[pos..].chars().next().unwrap();
            let prev_is_word = text[..pos].chars().next_back().is_some_and(is_js_word);
            if !prev_is_word
                && ch.is_ascii_uppercase()
                && let Some((word, len)) = match_cap_subtype_token(&text[pos..])
                && self.subtype_words.contains(word)
            {
                out.push_str("<SUBTYPE>");
                pos += len;
                continue;
            }
            out.push(ch);
            pos += ch.len_utf8();
        }
        out
    }
}

fn match_cap_subtype_token(s: &str) -> Option<(&str, usize)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_uppercase() {
        return None;
    }
    let mut end = 1usize;
    while end < bytes.len() && bytes[end].is_ascii_lowercase() {
        end += 1;
    }
    if end < 2 {
        return None;
    }
    if end + 2 < bytes.len() && bytes[end] == b'-' && bytes[end + 1].is_ascii_uppercase() {
        let mut h_end = end + 2;
        while h_end < bytes.len() && bytes[h_end].is_ascii_lowercase() {
            h_end += 1;
        }
        if h_end > end + 2 {
            end = h_end;
        }
    }
    if s[end..].chars().next().is_some_and(is_js_word) {
        return None;
    }
    Some((&s[..end], end))
}

fn replace_braced_costs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(rel) = text[i + 1..].find('}')
            && rel > 0
        {
            let mut end = i + 2 + rel;
            while end < bytes.len() && bytes[end] == b'{' {
                if let Some(rel2) = text[end + 1..].find('}')
                    && rel2 > 0
                {
                    end = end + 2 + rel2;
                } else {
                    break;
                }
            }
            out.push_str("{COST}");
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn replace_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let ch = text[pos..].chars().next().unwrap();
        if matches!(ch, '+' | '-' | '−') {
            let sign_len = ch.len_utf8();
            let rest = &text[pos + sign_len..];
            let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digit_len > 0 {
                let end = pos + sign_len + digit_len;
                let next_is_word = text[end..].chars().next().is_some_and(is_js_word);
                if !next_is_word {
                    out.push('N');
                    pos = end;
                    continue;
                }
            }
        } else if ch.is_ascii_digit() {
            let prev_is_word = text[..pos].chars().next_back().is_some_and(is_js_word);
            if !prev_is_word {
                let digit_len = text[pos..]
                    .bytes()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                let end = pos + digit_len;
                let next_is_word = text[end..].chars().next().is_some_and(is_js_word);
                if !next_is_word {
                    out.push('N');
                    pos = end;
                    continue;
                }
            }
        }
        out.push(ch);
        pos += ch.len_utf8();
    }
    out
}

fn replace_number_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    while pos < text.len() {
        let ch = text[pos..].chars().next().unwrap();
        let prev_is_word = text[..pos].chars().next_back().is_some_and(is_js_word);
        if !prev_is_word && ch.is_ascii_alphabetic() {
            let rest = &text[pos..];
            let mut matched_len = None;
            for nw in TEMPLATE_NUMBER_WORDS {
                if rest.len() >= nw.len()
                    && rest.is_char_boundary(nw.len())
                    && rest[..nw.len()].eq_ignore_ascii_case(nw)
                    && rest[nw.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !is_js_word(c))
                {
                    matched_len = Some(nw.len());
                    break;
                }
            }
            if let Some(len) = matched_len {
                out.push('N');
                pos += len;
                continue;
            }
        }
        out.push(ch);
        pos += ch.len_utf8();
    }
    out
}
