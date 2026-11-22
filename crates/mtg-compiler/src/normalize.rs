use std::borrow::Cow;

use mtg_ir::Span;

use crate::input::{RawCardInput, RawFaceInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedSentence {
    pub text: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedLine {
    pub raw_index: usize,
    pub raw: String,
    pub text: String,
    pub span: Span,
    pub ability_word: Option<String>,
    pub body: String,
    pub body_span: Span,
    pub sentences: Vec<NormalizedSentence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedFace {
    pub text: String,
    pub lines: Vec<NormalizedLine>,
}

pub const SELF_REFERENCE_TYPES: [&str; 22] = [
    "creature",
    "permanent",
    "spell",
    "card",
    "artifact",
    "enchantment",
    "land",
    "equipment",
    "aura",
    "vehicle",
    "mount",
    "spacecraft",
    "planet",
    "planeswalker",
    "battle",
    "saga",
    "class",
    "case",
    "siege",
    "room",
    "omen",
    "token",
];

pub const ABILITY_WORDS: [&str; 66] = [
    "adamant",
    "addendum",
    "alliance",
    "battalion",
    "battlefront",
    "bloodrush",
    "celebration",
    "channel",
    "chroma",
    "cohort",
    "constellation",
    "converge",
    "council's dilemma",
    "coven",
    "delirium",
    "descend 4",
    "descend 8",
    "disappear",
    "domain",
    "eerie",
    "eminence",
    "enrage",
    "fateful hour",
    "fathomless descent",
    "federation",
    "ferocious",
    "flurry",
    "formidable",
    "grandeur",
    "hellbent",
    "heroic",
    "imprint",
    "infusion",
    "inspired",
    "join forces",
    "kinship",
    "landfall",
    "lieutenant",
    "magecraft",
    "metalcraft",
    "morbid",
    "opus",
    "pack tactics",
    "paradox",
    "parley",
    "radiance",
    "raid",
    "rally",
    "renew",
    "repartee",
    "revolt",
    "rulebreaker",
    "secret council",
    "seek out",
    "spell mastery",
    "strange new worlds",
    "strive",
    "survival",
    "sweep",
    "tempting offer",
    "threshold",
    "undergrowth",
    "valiant",
    "vivid",
    "void",
    "will of the council",
];

pub const RULES_SECTION_701_ACTIONS: [&str; 73] = [
    "activate",
    "attach",
    "behold",
    "cast",
    "counter",
    "create",
    "destroy",
    "discard",
    "double",
    "triple",
    "exchange",
    "exile",
    "fight",
    "goad",
    "investigate",
    "mill",
    "play",
    "regenerate",
    "reveal",
    "sacrifice",
    "scry",
    "search",
    "shuffle",
    "surveil",
    "tap and untap",
    "tap",
    "untap",
    "transform",
    "convert",
    "fateseal",
    "clash",
    "planeswalk",
    "set in motion",
    "abandon",
    "proliferate",
    "detain",
    "populate",
    "monstrosity",
    "vote",
    "bolster",
    "manifest",
    "support",
    "meld",
    "exert",
    "explore",
    "assemble",
    "adapt",
    "amass",
    "learn",
    "venture into the dungeon",
    "connive",
    "open an attraction",
    "roll to visit your attractions",
    "incubate",
    "the ring tempts you",
    "face a villainous choice",
    "time travel",
    "discover",
    "cloak",
    "collect evidence",
    "suspect",
    "forage",
    "manifest dread",
    "endure",
    "harness",
    "airbend",
    "earthbend",
    "waterbend",
    "blight",
    "heal",
    "recruit",
    "empower jace",
    "assimilate",
];

pub const RULES_SECTION_702_KEYWORDS: [&str; 206] = [
    "deathtouch",
    "defender",
    "double strike",
    "enchant",
    "equip",
    "first strike",
    "flash",
    "flying",
    "haste",
    "hexproof",
    "indestructible",
    "intimidate",
    "landwalk",
    "lifelink",
    "protection",
    "reach",
    "shroud",
    "trample",
    "vigilance",
    "ward",
    "banding",
    "rampage",
    "cumulative upkeep",
    "flanking",
    "phasing",
    "buyback",
    "shadow",
    "cycling",
    "typecycling",
    "landcycling",
    "basic landcycling",
    "echo",
    "horsemanship",
    "fading",
    "kicker",
    "multikicker",
    "flashback",
    "madness",
    "fear",
    "morph",
    "megamorph",
    "amplify",
    "provoke",
    "storm",
    "affinity",
    "entwine",
    "modular",
    "sunburst",
    "bushido",
    "soulshift",
    "splice",
    "offering",
    "ninjutsu",
    "epic",
    "convoke",
    "dredge",
    "transmute",
    "bloodthirst",
    "haunt",
    "replicate",
    "forecast",
    "graft",
    "recover",
    "ripple",
    "split second",
    "suspend",
    "vanishing",
    "absorb",
    "aura swap",
    "delve",
    "fortify",
    "frenzy",
    "gravestorm",
    "poisonous",
    "transfigure",
    "champion",
    "changeling",
    "evoke",
    "hideaway",
    "prowl",
    "reinforce",
    "conspire",
    "persist",
    "wither",
    "retrace",
    "devour",
    "exalted",
    "unearth",
    "cascade",
    "annihilator",
    "level up",
    "rebound",
    "umbra armor",
    "totem armor",
    "infect",
    "battle cry",
    "living weapon",
    "undying",
    "miracle",
    "soulbond",
    "overload",
    "scavenge",
    "unleash",
    "cipher",
    "evolve",
    "extort",
    "fuse",
    "bestow",
    "tribute",
    "dethrone",
    "hidden agenda",
    "outlast",
    "prowess",
    "dash",
    "exploit",
    "menace",
    "renown",
    "awaken",
    "devoid",
    "ingest",
    "myriad",
    "surge",
    "skulk",
    "emerge",
    "escalate",
    "melee",
    "crew",
    "fabricate",
    "partner",
    "undaunted",
    "improvise",
    "aftermath",
    "embalm",
    "eternalize",
    "afflict",
    "ascend",
    "assist",
    "jump-start",
    "mentor",
    "afterlife",
    "riot",
    "spectacle",
    "escape",
    "companion",
    "mutate",
    "encore",
    "boast",
    "foretell",
    "demonstrate",
    "daybound and nightbound",
    "daybound",
    "nightbound",
    "disturb",
    "decayed",
    "cleave",
    "training",
    "compleated",
    "reconfigure",
    "blitz",
    "casualty",
    "enlist",
    "read ahead",
    "ravenous",
    "squad",
    "space sculptor",
    "visit",
    "prototype",
    "living metal",
    "more than meets the eye",
    "for mirrodin!",
    "toxic",
    "backup",
    "bargain",
    "craft",
    "disguise",
    "solved",
    "plot",
    "saddle",
    "spree",
    "freerunning",
    "gift",
    "offspring",
    "impending",
    "exhaust",
    "max speed",
    "start your engines!",
    "harmonize",
    "mobilize",
    "job select",
    "tiered",
    "station",
    "warp",
    "∞ (infinity)",
    "mayhem",
    "web-slinging",
    "firebending",
    "sneak",
    "increment",
    "paradigm",
    "power-up",
    "teamwork",
    "storied",
    "beam me up",
    "choose a background",
    "doctor's companion",
    "friends forever",
];

pub const RULES_PARAM_PREFIXES: [&str; 64] = [
    "boast",
    "exhaust",
    "power-up",
    "craft",
    "splice",
    "cleave",
    "reconfigure",
    "transfigure",
    "aura swap",
    "more than meets the eye",
    "partner",
    "saddle",
    "crew",
    "equip",
    "ward",
    "cycling",
    "typecycling",
    "landcycling",
    "basic landcycling",
    "cumulative upkeep",
    "echo",
    "fading",
    "vanishing",
    "suspend",
    "madness",
    "miracle",
    "overload",
    "flashback",
    "foretell",
    "plot",
    "disguise",
    "morph",
    "megamorph",
    "bestow",
    "dash",
    "blitz",
    "evoke",
    "mutate",
    "ninjutsu",
    "prowl",
    "spectacle",
    "surge",
    "freerunning",
    "gift",
    "impending",
    "offspring",
    "spree",
    "tiered",
    "warp",
    "web-slinging",
    "harmonize",
    "adapt",
    "monstrosity",
    "outlast",
    "scavenge",
    "unearth",
    "buyback",
    "escape",
    "embalm",
    "eternalize",
    "encore",
    "transmute",
    "forecast",
    "reinforce",
];

pub const NON_CHARACTER_LEGEND_FIRST_WORDS: [&str; 122] = [
    "the",
    "lord",
    "lady",
    "king",
    "queen",
    "prince",
    "princess",
    "voice",
    "spirit",
    "avatar",
    "scion",
    "child",
    "son",
    "daughter",
    "champion",
    "patron",
    "warden",
    "keeper",
    "master",
    "guardian",
    "herald",
    "emissary",
    "archon",
    "angel",
    "demon",
    "dragon",
    "elder",
    "beast",
    "elemental",
    "knight",
    "soldier",
    "wizard",
    "god",
    "goddess",
    "bringer",
    "sliver",
    "flash",
    "space",
    "mutagen",
    "time",
    "blood",
    "fire",
    "ice",
    "soul",
    "mind",
    "hand",
    "land",
    "mana",
    "dark",
    "light",
    "shadow",
    "storm",
    "iron",
    "gold",
    "silver",
    "star",
    "sun",
    "moon",
    "night",
    "day",
    "death",
    "life",
    "war",
    "rage",
    "fury",
    "hope",
    "grace",
    "fate",
    "doom",
    "void",
    "chaos",
    "order",
    "wild",
    "true",
    "pure",
    "last",
    "first",
    "high",
    "great",
    "old",
    "new",
    "red",
    "blue",
    "black",
    "white",
    "green",
    "ancient",
    "eternal",
    "prime",
    "grand",
    "tiny",
    "giant",
    "little",
    "big",
    "young",
    "arch",
    "captain",
    "commander",
    "lieutenant",
    "ensign",
    "admiral",
    "general",
    "major",
    "colonel",
    "sergeant",
    "marshal",
    "minister",
    "senator",
    "mayor",
    "duke",
    "baron",
    "count",
    "earl",
    "doctor",
    "professor",
    "saint",
    "father",
    "mother",
    "brother",
    "sister",
    "uncle",
    "aunt",
];

pub const CONNECTOR_WORDS: [&str; 14] = [
    "the", "of", "and", "a", "an", "in", "on", "at", "to", "from", "da", "de", "von", "van",
];

const RANK_PREFIXES: [&str; 11] = [
    "Captain",
    "Commander",
    "Lieutenant",
    "Ensign",
    "Admiral",
    "General",
    "Major",
    "Colonel",
    "Sergeant",
    "Dr.",
    "Professor",
];

const SELF_POSSESSIVE_HER_NOUNS: [&str; 16] = [
    "name",
    "owner",
    "owner's",
    "owner’s",
    "controller",
    "controller's",
    "controller’s",
    "base power",
    "base toughness",
    "power",
    "toughness",
    "mana value",
    "additional cost",
    "sneak cost",
    "colors",
    "color",
];

const SELF_POSSESSIVE_HER_NOUNS_EXTRA: [&str; 4] = ["other types", "types", "abilities", "loyalty"];

const SELF_OBJECT_HER_FOLLOW_WORDS: [&str; 36] = [
    "to",
    "into",
    "onto",
    "from",
    "on",
    "with",
    "under",
    "in",
    "at",
    "by",
    "for",
    "as",
    "until",
    "this",
    "that",
    "instead",
    "transformed",
    "tapped",
    "untapped",
    "flipped",
    "face",
    "deal",
    "and",
    "or",
    "then",
    "unless",
    "if",
    "when",
    "whenever",
    "where",
    "first",
    "second",
    "third",
    "fourth",
    "fifth",
    "alone",
];

const FALLBACK_VERBS: [&str; 18] = [
    "enters",
    "leaves",
    "dies",
    "attacks",
    "blocks",
    "becomes",
    "is",
    "deals",
    "can't",
    "can’t",
    "gets",
    "gains",
    "has",
    "loses",
    "costs",
    "remains",
    "stays",
    "transforms",
];

const FALLBACK_VERBS_EXTRA: [&str; 1] = ["flips"];

const LEAD_WORDS: [&str; 18] = [
    "when",
    "whenever",
    "as",
    "if",
    "unless",
    "until",
    "while",
    "exile",
    "return",
    "put",
    "sacrifice",
    "destroy",
    "tap",
    "untap",
    "transform",
    "flip",
    "on",
    "to",
];
const LEAD_WORDS_EXTRA: [&str; 2] = ["with", "from"];

const LEAD_BLOCKED_AFTER: [&str; 8] = [
    "token",
    "tokens",
    "card",
    "cards",
    "creature",
    "creatures",
    "spell",
    "spells",
];

#[inline]
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn format_js_cmc(cmc: Option<f64>) -> String {
    let Some(v) = cmc else {
        return String::new();
    };
    if v.is_finite() && v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        v.to_string()
    }
}

fn faces_source_text(faces: &[RawFaceInput]) -> String {
    if faces.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (idx, f) in faces.iter().enumerate() {
        if idx > 0 {
            out.push('\u{0002}');
        }
        out.push_str(&f.name);
        out.push('\u{0001}');
        out.push_str(&f.mana_cost);
        out.push('\u{0001}');
        out.push_str(&format_js_cmc(f.cmc));
        out.push('\u{0001}');
        out.push_str(&f.type_line);
        out.push('\u{0001}');
        out.push_str(&f.oracle_text);
        out.push('\u{0001}');
        for c in &f.colors {
            out.push_str(c);
        }
        out.push('\u{0001}');
        if let Some(ind) = &f.color_indicator {
            for c in ind {
                out.push_str(c);
            }
        }
        out.push('\u{0001}');
        out.push_str(&f.keywords.join(","));
        out.push('\u{0001}');
        out.push_str(f.power.as_deref().unwrap_or(""));
        out.push('\u{0001}');
        out.push_str(f.toughness.as_deref().unwrap_or(""));
        out.push('\u{0001}');
        out.push_str(f.loyalty.as_deref().unwrap_or(""));
        out.push('\u{0001}');
        out.push_str(f.defense.as_deref().unwrap_or(""));
    }
    out
}

/// Canonical source text used for `text_hash` computation (matches TS `compiler/index.ts:133-149`).
/// Note: top-level `card.defense` is intentionally omitted to match TS exactly.
pub fn source_text(card: &RawCardInput) -> String {
    let mut out = String::new();
    out.push_str(card.layout.as_deref().unwrap_or(""));
    out.push('\u{0000}');
    out.push_str(&card.name);
    out.push('\u{0000}');
    out.push_str(&card.mana_cost);
    out.push('\u{0000}');
    out.push_str(&card.type_line);
    out.push('\u{0000}');
    out.push_str(&card.oracle_text);
    out.push('\u{0000}');
    out.push_str(card.power.as_deref().unwrap_or(""));
    out.push('\u{0000}');
    out.push_str(card.toughness.as_deref().unwrap_or(""));
    out.push('\u{0000}');
    out.push_str(card.loyalty.as_deref().unwrap_or(""));
    out.push('\u{0000}');
    for c in &card.colors {
        out.push_str(c);
    }
    out.push('\u{0000}');
    if let Some(ind) = &card.color_indicator {
        for c in ind {
            out.push_str(c);
        }
    }
    out.push('\u{0000}');
    out.push_str(&faces_source_text(&card.faces));
    out.push('\u{0000}');
    if let Some(b) = &card.back_face {
        out.push_str(&b.name);
        out.push('\u{0001}');
        out.push_str(&b.mana_cost);
        out.push('\u{0001}');
        out.push_str(&b.type_line);
        out.push('\u{0001}');
        out.push_str(&b.oracle_text);
    }
    out
}

/// FNV-1a 32-bit hash over UTF-16 code units, matching TS `charCodeAt(i)`.
pub fn hash_text(text: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for unit in text.encode_utf16() {
        h ^= u32::from(unit);
        h = h.wrapping_mul(0x0100_0193);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 8];
    for (i, slot) in buf.iter_mut().enumerate() {
        let shift = (7 - i) * 4;
        *slot = HEX[((h >> shift) & 0xf) as usize];
    }
    std::str::from_utf8(&buf).unwrap().to_owned()
}

/// Strips parenthesized reminder text (including nested parens) and collapses whitespace.
pub fn strip_reminder_text(line: &str) -> String {
    strip_reminder_text_cow(line).into_owned()
}

pub(crate) fn strip_reminder_text_cow(line: &str) -> Cow<'_, str> {
    if !line.as_bytes().contains(&b'(') {
        return collapse_ws_trim_cow(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut depth: u32 = 0;
    for (idx, ch) in line.char_indices() {
        if depth == 0 {
            if ch == '(' {
                // Check ahead for balanced closing paren
                let mut check_depth: u32 = 1;
                let mut balanced = false;
                for c2 in line[idx + ch.len_utf8()..].chars() {
                    if c2 == '(' {
                        check_depth += 1;
                    } else if c2 == ')' {
                        check_depth -= 1;
                        if check_depth == 0 {
                            balanced = true;
                            break;
                        }
                    }
                }
                if balanced {
                    while out.ends_with(char::is_whitespace) {
                        out.pop();
                    }
                    depth = 1;
                } else {
                    out.push('(');
                }
            } else {
                out.push(ch);
            }
        } else if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
        }
    }

    Cow::Owned(collapse_ws_trim(&out))
}

fn collapse_ws_trim_cow(s: &str) -> Cow<'_, str> {
    let trimmed = s.trim();
    let bytes = trimmed.as_bytes();
    let mut prev_space = false;
    let mut needs_rebuild = false;
    for &b in bytes {
        if b == b' ' {
            if prev_space {
                needs_rebuild = true;
                break;
            }
            prev_space = true;
        } else if b.is_ascii_whitespace() || b >= 0x80 {
            // Check conservatively if non-ASCII whitespace or non-space ASCII whitespace exists
            if b.is_ascii_whitespace() {
                needs_rebuild = true;
                break;
            }
            // Non-ASCII byte: let slow path check if whitespace (rare)
            if trimmed.chars().any(|c| c.is_whitespace() && c != ' ') {
                needs_rebuild = true;
            }
            break;
        } else {
            prev_space = false;
        }
    }
    if !needs_rebuild && !bytes.windows(2).any(|w| w == b"  ") {
        // Check if non-ASCII had double spaces after first non-ASCII
        if bytes.iter().all(|&b| b < 0x80) || !trimmed.contains("  ") {
            return Cow::Borrowed(trimmed);
        }
    }
    Cow::Owned(collapse_ws_trim(trimmed))
}

fn collapse_ws_trim(s: &str) -> String {
    let mut collapsed = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
                prev_ws = true;
            }
        } else {
            collapsed.push(ch);
            prev_ws = false;
        }
    }
    collapsed
}

fn add_unique_name(names: &mut Vec<String>, cand: String) {
    if !cand.is_empty() && !names.iter().any(|existing| existing == &cand) {
        names.push(cand);
    }
}

fn is_letter_or_dot(c: char) -> bool {
    c.is_alphabetic() || c == '.'
}

fn is_name_word_char(c: char) -> bool {
    c.is_alphabetic() || matches!(c, '\'' | '’' | '-')
}

fn is_cap_name_word_min3(w: &str) -> bool {
    let mut chars = w.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_uppercase() {
        return false;
    }
    let rest_count = chars.clone().count();
    rest_count >= 2 && chars.all(is_name_word_char)
}

fn parse_nick_title(name: &str) -> Option<String> {
    let first_quote = name.find('"')?;
    let second_quote_rel = name[first_quote + 1..].find('"')?;
    let before = name[..first_quote].trim_end();
    let after = name[first_quote + 1 + second_quote_rel + 1..].trim();
    if before.is_empty() || after.is_empty() {
        return None;
    }
    // `after` must be single word matching [\p{Lu}][\p{L}'’-]+
    let mut after_chars = after.chars();
    let a0 = after_chars.next()?;
    if !a0.is_uppercase() || after_chars.clone().count() < 1 || !after_chars.all(is_name_word_char)
    {
        return None;
    }
    // `before` must be "<First> <Middle>"
    let before_words: Vec<&str> = before.split_whitespace().collect();
    if before_words.len() != 2 {
        return None;
    }
    let first_w = before_words[0];
    let middle_w = before_words[1];
    let mut f_chars = first_w.chars();
    let f0 = f_chars.next()?;
    if !f0.is_uppercase() || f_chars.clone().count() < 1 || !f_chars.all(is_letter_or_dot) {
        return None;
    }
    if middle_w.is_empty() || !middle_w.chars().all(is_name_word_char) {
        return None;
    }
    Some(format!("{first_w} {after}"))
}

fn parse_rank_surname(base_name: &str) -> Option<String> {
    let words: Vec<&str> = base_name.split_whitespace().collect();
    if words.len() < 3 {
        return None;
    }
    let rank = words[0];
    if !RANK_PREFIXES.contains(&rank) {
        return None;
    }
    let surname = *words.last()?;
    if !is_cap_name_word_min3(surname) {
        return None;
    }
    for mid in &words[1..words.len() - 1] {
        let mut chars = mid.chars();
        let c0 = chars.next()?;
        if !c0.is_uppercase() || !chars.all(|c| is_name_word_char(c) || c == '.') {
            return None;
        }
    }
    Some(format!("{rank} {surname}"))
}

fn is_roman_numeral_suffix(w: &str) -> bool {
    matches!(
        w,
        "II" | "III" | "IV" | "VI" | "VII" | "VIII" | "IX" | "XI" | "XII" | "XL" | "XLVIII"
    ) || (w.len() >= 2 && w.bytes().all(|b| matches!(b, b'I' | b'V' | b'X')))
}

fn parse_roman_legend_name(name: &str) -> Option<String> {
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() < 2 {
        return None;
    }
    let last = *words.last()?;
    if !is_roman_numeral_suffix(last) {
        return None;
    }
    let prefix_words = &words[..words.len() - 1];
    if !prefix_words.iter().all(|w| is_cap_name_word_min3(w)) {
        return None;
    }
    let first_low = prefix_words[0].to_lowercase();
    if NON_CHARACTER_LEGEND_FIRST_WORDS.contains(&first_low.as_str()) {
        return None;
    }
    Some(prefix_words.join(" "))
}

fn parse_epithet_name(name: &str) -> Option<String> {
    let words: Vec<&str> = name.split_whitespace().collect();
    for split_len in [1usize, 2usize] {
        if words.len() <= split_len + 1 {
            continue;
        }
        if !words[..split_len].iter().all(|w| is_cap_name_word_min3(w)) {
            continue;
        }
        let rest = &words[split_len..];
        let next_after_conn = if rest[0] == "the" && rest.len() >= 2 {
            Some(rest[1])
        } else if rest[0] == "of" {
            if rest.len() >= 3 && (rest[1] == "the" || rest[1] == "a") {
                Some(rest[2])
            } else if rest.len() >= 2 {
                Some(rest[1])
            } else {
                None
            }
        } else if rest.len() >= 3 && rest[0] == "and" && rest[1] == "the" {
            Some(rest[2])
        } else {
            None
        };
        if let Some(after) = next_after_conn
            && after.chars().next().is_some_and(char::is_uppercase)
        {
            let first_low = words[0].to_lowercase();
            if !NON_CHARACTER_LEGEND_FIRST_WORDS.contains(&first_low.as_str()) {
                return Some(words[..split_len].join(" "));
            }
            return None;
        }
    }
    None
}

fn ends_with_ci_word(prefix: &str, word: &str) -> bool {
    let trimmed = prefix.trim_end();
    if trimmed.len() == prefix.len() {
        return false; // must have trailing whitespace after word
    }
    if trimmed.len() < word.len() {
        return false;
    }
    let start = trimmed.len() - word.len();
    if !trimmed.is_char_boundary(start) {
        return false;
    }
    if !trimmed[start..].eq_ignore_ascii_case(word) {
        return false;
    }
    start == 0
        || !trimmed[..start]
            .chars()
            .next_back()
            .is_some_and(is_word_char)
}

fn starts_with_ci_word(s: &str, word: &str) -> Option<usize> {
    if s.len() < word.len() || !s.is_char_boundary(word.len()) {
        return None;
    }
    if !s[..word.len()].eq_ignore_ascii_case(word) {
        return None;
    }
    let next = s[word.len()..].chars().next();
    if next.is_some_and(is_word_char) {
        return None;
    }
    Some(word.len())
}

fn is_possessive_followed_by_token_name(tail: &str) -> bool {
    let rest = if let Some(r) = tail.strip_prefix('\'') {
        r
    } else if let Some(r) = tail.strip_prefix('’') {
        r
    } else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('s') else {
        return false;
    };
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        return false; // need \s+
    }
    let Some(comma) = trimmed.find(',') else {
        return false;
    };
    let token_word = &trimmed[..comma];
    let mut chars = token_word.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_uppercase() || !chars.all(is_name_word_char) {
        return false;
    }
    let after_comma = trimmed[comma + 1..].trim_start();
    starts_with_ci_word(after_comma, "a").is_some()
}

fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

fn replace_name_occurrences<'a>(
    text: &'a str,
    sorted_names: &[String],
    full_name: &str,
) -> Cow<'a, str> {
    let mut out: Cow<'a, str> = Cow::Borrowed(text);
    for n in sorted_names {
        if n.is_empty() || !out.contains(n.as_str()) {
            continue;
        }
        let mut pos = 0usize;
        let mut rebuilt = String::with_capacity(out.len());
        let check_named = n != full_name;
        loop {
            let Some(rel) = out[pos..].find(n.as_str()) else {
                rebuilt.push_str(&out[pos..]);
                break;
            };
            let idx = pos + rel;
            let after_idx = idx + n.len();
            let before = out[..idx].chars().next_back();
            let after = out[after_idx..].chars().next();
            let ok_left = before.is_none_or(|c| !is_word_char(c));
            let ok_right = after.is_none_or(|c| !is_word_char(c));

            let is_named = if check_named {
                let prefix_start = floor_char_boundary(&out, idx.saturating_sub(32));
                let prefix = &out[prefix_start..idx];
                let tail_end = ceil_char_boundary(&out, (after_idx + 64).min(out.len()));
                let tail = &out[after_idx..tail_end];
                ends_with_ci_word(prefix, "named")
                    || (ends_with_ci_word(prefix, "create")
                        && is_possessive_followed_by_token_name(tail))
            } else {
                false
            };

            rebuilt.push_str(&out[pos..idx]);
            if ok_left && ok_right && !is_named {
                rebuilt.push('~');
            } else {
                rebuilt.push_str(n);
            }
            pos = after_idx;
        }
        out = Cow::Owned(rebuilt);
    }
    out
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

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn has_fallback_self_context(remaining: &str, cand: &str) -> bool {
    if cand.is_empty() || !remaining.contains(cand) {
        return false;
    }
    let mut pos = 0usize;
    loop {
        let Some(rel) = remaining[pos..].find(cand) else {
            return false;
        };
        let idx = pos + rel;
        let after_idx = idx + cand.len();
        let before = remaining[..idx].chars().next_back();
        let ok_left = before.is_none_or(|c| !is_word_char(c));
        let prefix_start = floor_char_boundary(remaining, idx.saturating_sub(32));
        let is_named = ends_with_ci_word(&remaining[prefix_start..idx], "named");
        if ok_left && !is_named {
            let after = remaining[after_idx..].chars().next();
            if after.is_none_or(|c| !is_word_char(c)) {
                let tail = &remaining[after_idx..];
                // Check possessive or verb after
                if tail
                    .strip_prefix("'s")
                    .or_else(|| tail.strip_prefix("’s"))
                    .is_some_and(|r| !r.chars().next().is_some_and(is_word_char))
                {
                    return true;
                }
                let tail_trim = tail.trim_start();
                if tail_trim.len() < tail.len() {
                    for verb in FALLBACK_VERBS.iter().chain(FALLBACK_VERBS_EXTRA.iter()) {
                        if let Some(len) = starts_with_ci_word(tail_trim, verb) {
                            let _ = len;
                            return true;
                        }
                        if tail_trim.starts_with(verb)
                            && !tail_trim[verb.len()..]
                                .chars()
                                .next()
                                .is_some_and(is_word_char)
                        {
                            return true;
                        }
                    }
                }
                // Check lead before and non-blocked after
                let head = &remaining[..idx];
                let lead_before = head.is_empty()
                    || head.ends_with('\n')
                    || matches!(head.trim_end().chars().next_back(), Some('.' | ';' | ':'))
                    || LEAD_WORDS
                        .iter()
                        .chain(LEAD_WORDS_EXTRA.iter())
                        .any(|w| ends_with_ci_word(head, w));
                let lead_after_ok = matches!(
                    after,
                    None | Some(' ' | '\t' | '\n' | '\r' | '.' | ',' | ';' | ':' | '!' | '?')
                ) && {
                    let after_ws = tail.trim_start();
                    if after_ws.len() < tail.len() {
                        !LEAD_BLOCKED_AFTER
                            .iter()
                            .any(|blk| starts_with_ci_word(after_ws, blk).is_some())
                    } else {
                        true
                    }
                };
                if lead_before && lead_after_ok {
                    return true;
                }
            }
        }
        pos = after_idx;
    }
}

fn add_character_legend_fallback_names(
    names: &mut Vec<String>,
    name: &str,
    face_oracle_text: &str,
) {
    let stripped = strip_reminder_text_cow(face_oracle_text);
    let mut sorted = names.clone();
    sorted.sort_by_key(|n| std::cmp::Reverse(n.encode_utf16().count()));
    let remaining = replace_name_occurrences(&stripped, &sorted, name);

    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() < 2 {
        return;
    }
    let w0 = words[0];
    let w1 = words[1];
    let mut candidates = Vec::new();
    if words.len() >= 3
        && is_cap_name_word_min3(w0)
        && is_cap_name_word_min3(w1)
        && !CONNECTOR_WORDS.contains(&w0.to_lowercase().as_str())
        && !CONNECTOR_WORDS.contains(&w1.to_lowercase().as_str())
        && !NON_CHARACTER_LEGEND_FIRST_WORDS.contains(&w0.to_lowercase().as_str())
    {
        candidates.push(format!("{w0} {w1}"));
    }
    if is_cap_name_word_min3(w0)
        && !w0.ends_with('.')
        && !CONNECTOR_WORDS.contains(&w0.to_lowercase().as_str())
        && !NON_CHARACTER_LEGEND_FIRST_WORDS.contains(&w0.to_lowercase().as_str())
    {
        candidates.push(w0.to_string());
    }
    for cand in candidates {
        if !names.contains(&cand) && has_fallback_self_context(&remaining, &cand) {
            add_unique_name(names, cand);
        }
    }
}

/// Computes all self-reference name forms for a card face (full name, short names, epithets, rank/surname).
pub fn self_names(
    name: &str,
    is_character_legend: bool,
    face_oracle_text: Option<&str>,
) -> Vec<String> {
    let mut names = vec![name.to_string()];
    let comma = name.find(", ");
    if let Some(c_idx) = comma
        && c_idx > 0
    {
        add_unique_name(&mut names, name[..c_idx].to_string());
    }
    if let Some(nick) = parse_nick_title(name) {
        add_unique_name(&mut names, nick);
    }
    if is_character_legend {
        let base_name = if let Some(c_idx) = comma
            && c_idx > 0
        {
            &name[..c_idx]
        } else {
            name
        };
        if let Some(rs) = parse_rank_surname(base_name) {
            add_unique_name(&mut names, rs);
        }
    }
    if is_character_legend && comma.is_none() {
        if let Some(roman) = parse_roman_legend_name(name) {
            add_unique_name(&mut names, roman);
        }
        if let Some(ep) = parse_epithet_name(name) {
            add_unique_name(&mut names, ep);
        }
        if let Some(oracle) = face_oracle_text {
            add_character_legend_fallback_names(&mut names, name, oracle);
        }
    }
    names
}

fn replace_this_self_references(line: &str) -> Cow<'_, str> {
    if !contains_ascii_ci(line.as_bytes(), b"this ") {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut pos = 0usize;
    let bytes = line.as_bytes();
    let mut changed = false;
    while pos < bytes.len() {
        let ch = line[pos..].chars().next().unwrap();
        let prev = line[..pos].chars().next_back();
        if prev.is_none_or(|c| !is_word_char(c))
            && let Some(this_len) = starts_with_ci_word(&line[pos..], "this")
        {
            // Actually `starts_with_ci_word` requires non-word char right after "this",
            // so if the next char is ' ', check `this <noun>`.
            let after_this = &line[pos + this_len..];
            if let Some(after_space) = after_this.strip_prefix(' ') {
                let mut matched_noun: Option<usize> = None;
                for noun in SELF_REFERENCE_TYPES {
                    if let Some(nlen) = starts_with_ci_word(after_space, noun) {
                        matched_noun = Some(nlen);
                        break;
                    }
                }
                if let Some(nlen) = matched_noun {
                    out.push('~');
                    pos += this_len + 1 + nlen;
                    changed = true;
                    continue;
                }
            }
        }
        out.push(ch);
        pos += ch.len_utf8();
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(line)
    }
}

fn preserve_case_it(matched: &str, lower_replacement: &str) -> String {
    let first = matched.chars().next();
    if let Some(c) = first
        && c.is_uppercase()
    {
        let mut chars = lower_replacement.chars();
        if let Some(r0) = chars.next() {
            return format!("{}{}", r0.to_uppercase(), chars.as_str());
        }
    }
    lower_replacement.to_string()
}

fn matches_ci_phrase(s: &str, phrase: &str) -> bool {
    let mut si = s.char_indices().peekable();
    for pc in phrase.chars() {
        if pc == ' ' {
            let mut saw_ws = false;
            while let Some(&(_, sc)) = si.peek() {
                if sc.is_whitespace() {
                    saw_ws = true;
                    si.next();
                } else {
                    break;
                }
            }
            if !saw_ws {
                return false;
            }
        } else {
            let Some((_, sc)) = si.next() else {
                return false;
            };
            if !sc.eq_ignore_ascii_case(&pc) {
                return false;
            }
        }
    }
    match si.peek() {
        None => true,
        Some(&(_, next_c)) => !is_word_char(next_c),
    }
}

fn is_self_possessive_her_follow(tail: &str) -> bool {
    let after_ws = tail.trim_start();
    if after_ws.len() == tail.len() {
        return false;
    }
    SELF_POSSESSIVE_HER_NOUNS
        .iter()
        .chain(SELF_POSSESSIVE_HER_NOUNS_EXTRA.iter())
        .any(|noun| matches_ci_phrase(after_ws, noun))
}

fn is_self_object_her_follow(tail: &str) -> bool {
    let Some(first) = tail.chars().next() else {
        return true; // end of segment
    };
    if matches!(
        first,
        '.' | ',' | ';' | ':' | '!' | '?' | '"' | '”' | '\'' | '’' | ')'
    ) {
        return true;
    }
    let after_ws = tail.trim_start();
    if after_ws.len() == tail.len() {
        return false;
    }
    SELF_OBJECT_HER_FOLLOW_WORDS
        .iter()
        .any(|w| starts_with_ci_word(after_ws, w).is_some())
}

fn normalize_self_pronouns_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    let mut pos = 0usize;
    while pos < seg.len() {
        let ch = seg[pos..].chars().next().unwrap();
        let prev = seg[..pos].chars().next_back();
        if prev.is_none_or(|c| !is_word_char(c)) {
            let rest = &seg[pos..];
            if let Some(len) = starts_with_ci_word(rest, "himself")
                .or_else(|| starts_with_ci_word(rest, "herself"))
            {
                out.push_str(&preserve_case_it(&rest[..len], "itself"));
                pos += len;
                continue;
            }
            if let Some(len) = starts_with_ci_word(rest, "he's")
                .or_else(|| starts_with_ci_word(rest, "he’s"))
                .or_else(|| starts_with_ci_word(rest, "she's"))
                .or_else(|| starts_with_ci_word(rest, "she’s"))
            {
                out.push_str(&preserve_case_it(&rest[..len], "it's"));
                pos += len;
                continue;
            }
            if let Some(len) = starts_with_ci_word(rest, "her") {
                let tail = &rest[len..];
                if is_self_possessive_her_follow(tail) {
                    out.push_str(&preserve_case_it(&rest[..len], "its"));
                    pos += len;
                    continue;
                }
                if is_self_object_her_follow(tail) {
                    out.push_str(&preserve_case_it(&rest[..len], "it"));
                    pos += len;
                    continue;
                }
            }
            if let Some(len) = starts_with_ci_word(rest, "his") {
                out.push_str(&preserve_case_it(&rest[..len], "its"));
                pos += len;
                continue;
            }
            if let Some(len) = starts_with_ci_word(rest, "him") {
                out.push_str(&preserve_case_it(&rest[..len], "it"));
                pos += len;
                continue;
            }
            if let Some(len) =
                starts_with_ci_word(rest, "he").or_else(|| starts_with_ci_word(rest, "she"))
            {
                out.push_str(&preserve_case_it(&rest[..len], "it"));
                pos += len;
                continue;
            }
        }
        out.push(ch);
        pos += ch.len_utf8();
    }
    out
}

fn contains_any_self_pronoun(text: &str) -> bool {
    let bytes = text.as_bytes();
    if !contains_ascii_ci(bytes, b"he") && !contains_ascii_ci(bytes, b"hi") {
        return false;
    }
    let mut pos = 0usize;
    while pos < text.len() {
        let ch = text[pos..].chars().next().unwrap();
        let b0 = ch as u32;
        if (b0 == b'h' as u32 || b0 == b'H' as u32 || b0 == b's' as u32 || b0 == b'S' as u32)
            && text[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !is_word_char(c))
        {
            let rest = &text[pos..];
            for p in ["he", "she", "him", "her", "his", "himself", "herself"] {
                if starts_with_ci_word(rest, p).is_some() {
                    return true;
                }
            }
        }
        pos += ch.len_utf8();
    }
    false
}

fn normalize_self_pronouns_outside_quotes(text: &str) -> Cow<'_, str> {
    if !contains_any_self_pronoun(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut outside_start = 0usize;
    let mut scan = 0usize;
    while scan < text.len() {
        let rest = &text[scan..];
        let next_ascii = rest.find('"');
        let next_curly = rest.find('“');
        let quote_start = match (next_ascii, next_curly) {
            (Some(a), Some(b)) => Some((a.min(b), if a <= b { '"' } else { '”' })),
            (Some(a), None) => Some((a, '"')),
            (None, Some(b)) => Some((b, '”')),
            (None, None) => None,
        };
        let Some((rel_q, close_ch)) = quote_start else {
            break;
        };
        let q_abs = scan + rel_q;
        let open_len = text[q_abs..].chars().next().unwrap().len_utf8();
        let after_open = &text[q_abs + open_len..];
        if let Some(rel_close) = after_open.find(close_ch) {
            out.push_str(&normalize_self_pronouns_segment(
                &text[outside_start..q_abs],
            ));
            let quoted_end = q_abs + open_len + rel_close + close_ch.len_utf8();
            out.push_str(&text[q_abs..quoted_end]);
            outside_start = quoted_end;
            scan = quoted_end;
        } else {
            scan = q_abs + open_len;
        }
    }
    if outside_start < text.len() {
        out.push_str(&normalize_self_pronouns_segment(&text[outside_start..]));
    }
    Cow::Owned(out)
}

fn replace_self_references_with_names(line: &str, name: &str, sorted_names: &[String]) -> String {
    let out = replace_name_occurrences(line, sorted_names, name);
    let out2 = replace_this_self_references(&out);
    normalize_self_pronouns_outside_quotes(&out2).into_owned()
}

pub fn replace_self_references(
    line: &str,
    name: &str,
    is_character_legend: bool,
    face_oracle_text: Option<&str>,
) -> String {
    let mut sorted_names = self_names(name, is_character_legend, face_oracle_text);
    sorted_names.sort_by_key(|n| std::cmp::Reverse(n.encode_utf16().count()));
    replace_self_references_with_names(line, name, &sorted_names)
}

fn is_saga_roman_list(low: &str) -> bool {
    let mut parts = low.split(',');
    let mut any = false;
    for part in &mut parts {
        let p = part.trim();
        if p.is_empty() || !p.bytes().all(|b| matches!(b, b'i' | b'v' | b'x')) {
            return false;
        }
        any = true;
    }
    any
}

/// Returns `true` if `raw_prefix` (before `" — "`) is an ability or flavor word (CR 207.2c/d).
pub fn is_ability_or_flavor_word(raw_prefix: &str) -> bool {
    let low = raw_prefix.replace('’', "'").to_lowercase();
    let low = low.trim();
    if ABILITY_WORDS.contains(&low) {
        return true;
    }
    if is_saga_roman_list(low) {
        return false;
    }
    if RULES_SECTION_701_ACTIONS.contains(&low) || RULES_SECTION_702_KEYWORDS.contains(&low) {
        return false;
    }
    if low.ends_with("cycling") {
        return false;
    }
    for p in RULES_PARAM_PREFIXES {
        if low.len() > p.len() && low.starts_with(p) {
            let rest = &low[p.len()..];
            if rest.starts_with(' ') || rest.starts_with('—') {
                return false;
            }
        }
    }
    true
}

fn is_ability_word_char(c: char) -> bool {
    c.is_alphabetic() || c.is_ascii_digit() || matches!(c, '\'' | '’' | '!' | ',' | ' ' | '-')
}

fn is_bullet_ability_word_char(c: char) -> bool {
    c.is_alphabetic()
        || c.is_ascii_digit()
        || matches!(c, '\'' | '’' | '!' | '?' | ',' | '.' | '~' | ' ' | '-')
}

fn match_ability_word_prefix(text: &str) -> Option<(String, usize)> {
    let sep = " — ";
    // Standard ABILITY_WORD: ^([\p{Lu}0-9][\p{L}0-9'’!, -]*?) — (?=[\p{Lu}•{~])
    if let Some(c0) = text.chars().next()
        && (c0.is_uppercase() || c0.is_ascii_digit())
        && let Some(sep_idx) = text.find(sep)
    {
        let prefix = &text[..sep_idx];
        let after_idx = sep_idx + sep.len();
        let next_ch = text[after_idx..].chars().next();
        if !prefix.is_empty()
            && prefix.chars().all(is_ability_word_char)
            && next_ch.is_some_and(|c| c.is_uppercase() || matches!(c, '•' | '{' | '~'))
            && is_ability_or_flavor_word(prefix)
        {
            return Some((prefix.to_string(), after_idx));
        }
    }

    // BULLET_ABILITY_WORD: ^•\s+([\p{L}0-9~][\p{L}0-9'’!?,.~ -]*?) — (?!\{[^}]+\}\s*—)(?=[\p{Lu}0-9{~])
    if let Some(after_bullet) = text.strip_prefix('•') {
        let after_ws = after_bullet.trim_start();
        if after_ws.len() < after_bullet.len()
            && let Some(c0) = after_ws.chars().next()
            && (c0.is_alphabetic() || c0.is_ascii_digit() || c0 == '~')
            && let Some(rel_sep) = after_ws.find(sep)
        {
            let prefix = &after_ws[..rel_sep];
            let body_start = (text.len() - after_ws.len()) + rel_sep + sep.len();
            let after_sep = &text[body_start..];
            let next_ch = after_sep.chars().next();
            let double_dash_cost = if let Some(inner) = after_sep.strip_prefix('{')
                && let Some(close) = inner.find('}')
                && close > 0
            {
                inner[close + 1..].trim_start().starts_with('—')
            } else {
                false
            };
            let prefix_low = prefix.to_ascii_lowercase();
            let is_villainous = prefix_low.ends_with("faces a villainous choice")
                && prefix_low
                    .strip_suffix("faces a villainous choice")
                    .is_some_and(|h| {
                        h.is_empty() || h.chars().next_back().is_some_and(|c| !is_word_char(c))
                    });
            let starts_pawprint = prefix_low.starts_with("{p}");

            if !prefix.is_empty()
                && prefix.chars().all(is_bullet_ability_word_char)
                && !double_dash_cost
                && next_ch.is_some_and(|c| {
                    c.is_uppercase() || c.is_ascii_digit() || matches!(c, '{' | '~')
                })
                && !is_villainous
                && !starts_pawprint
                && is_ability_or_flavor_word(prefix)
            {
                return Some((prefix.to_string(), body_start));
            }
        }
    }

    None
}

/// Splits text into sentences outside quotation marks, preserving UTF-8 byte offsets.
pub fn split_sentences(text: &str, base_offset: u32) -> Vec<NormalizedSentence> {
    let mut out = Vec::new();
    let mut in_quote = false;
    let mut in_single_quote = false;
    let mut start = 0usize;
    let mut iter = text.char_indices().peekable();

    while let Some((i, ch)) = iter.next() {
        if !in_single_quote && matches!(ch, '"' | '“' | '”') {
            in_quote = match ch {
                '“' => true,
                '”' => false,
                _ => !in_quote,
            };
        } else if !in_quote && matches!(ch, '\'' | '‘' | '’') {
            let prev_is_space = i == 0 || text[..i].ends_with(' ');
            if !in_single_quote && prev_is_space {
                in_single_quote = true;
            } else if in_single_quote {
                let next_is_alpha = iter.peek().is_some_and(|&(_, nc)| nc.is_ascii_alphabetic());
                if !next_is_alpha {
                    in_single_quote = false;
                }
            }
        }
        if in_quote || in_single_quote {
            continue;
        }
        let next_char = iter.peek().map(|&(_, nc)| nc);
        if (ch == '.' || ch == '!') && (next_char.is_none() || next_char == Some(' ')) {
            let end = i + ch.len_utf8();
            push_sentence(&mut out, text, start, end, base_offset);
            start = end;
        }
    }
    push_sentence(&mut out, text, start, text.len(), base_offset);
    out
}

fn push_sentence(
    out: &mut Vec<NormalizedSentence>,
    text: &str,
    start: usize,
    end: usize,
    base_offset: u32,
) {
    let mut s = start;
    while s < end && text.as_bytes()[s] == b' ' {
        s += 1;
    }
    if s >= end {
        return;
    }
    out.push(NormalizedSentence {
        text: text[s..end].to_string(),
        span: Span {
            start: base_offset + s as u32,
            end: base_offset + end as u32,
        },
    });
}

/// Splits sentences at closing quotes ending in `.`, `!`, or `?` followed by whitespace and uppercase.
pub fn split_quoted_sentence_endings(sentences: &[NormalizedSentence]) -> Vec<NormalizedSentence> {
    let mut out = Vec::with_capacity(sentences.len());
    for sent in sentences {
        if !sent.text.contains(['"', '“', '”']) {
            out.push(sent.clone());
            continue;
        }
        let s = &sent.text;
        let mut in_quotes = false;
        let mut start = 0usize;
        let mut i = 0usize;
        let mut prev_ch: Option<char> = None;
        while i < s.len() {
            let ch = s[i..].chars().next().unwrap();
            let ch_len = ch.len_utf8();
            let is_closing_quote = in_quotes && matches!(ch, '"' | '”');
            if is_closing_quote && prev_ch.is_some_and(|p| matches!(p, '.' | '!' | '?')) {
                let after_quote = i + ch_len;
                let mut ws_count = 0usize;
                let mut next = after_quote;
                for (rel, wc) in s[after_quote..].char_indices() {
                    if wc.is_whitespace() && ws_count < 16 {
                        ws_count += 1;
                        next = after_quote + rel + wc.len_utf8();
                    } else {
                        break;
                    }
                }
                if ws_count > 0
                    && s[next..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                {
                    let left = s[start..after_quote].trim();
                    if !left.is_empty() {
                        let trim_start = s[start..after_quote]
                            .find(|c: char| !c.is_whitespace())
                            .unwrap_or(0);
                        out.push(NormalizedSentence {
                            text: left.to_string(),
                            span: Span {
                                start: sent.span.start + (start + trim_start) as u32,
                                end: sent.span.start + after_quote as u32,
                            },
                        });
                    }
                    start = next;
                    in_quotes = false;
                    prev_ch = None;
                    i = next;
                    continue;
                }
            }
            match ch {
                '“' => in_quotes = true,
                '”' => in_quotes = false,
                '"' => in_quotes = !in_quotes,
                _ => {}
            }
            prev_ch = Some(ch);
            i += ch_len;
        }
        if start < s.len() {
            let tail = s[start..].trim();
            if !tail.is_empty() {
                let trim_start = s[start..].find(|c: char| !c.is_whitespace()).unwrap_or(0);
                let trim_end = s[start..]
                    .rfind(|c: char| !c.is_whitespace())
                    .map(|idx| idx + s[start + idx..].chars().next().unwrap().len_utf8())
                    .unwrap_or(s.len() - start);
                out.push(NormalizedSentence {
                    text: tail.to_string(),
                    span: Span {
                        start: sent.span.start + (start + trim_start) as u32,
                        end: sent.span.start + (start + trim_end) as u32,
                    },
                });
            }
        }
    }
    out
}

/// Normalizes a face's rules text into lines and sentences with UTF-8 byte spans.
pub fn normalize_face(name: &str, rules_text: &str, is_character_legend: bool) -> NormalizedFace {
    let mut lines = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut offset: u32 = 0;

    let mut sorted_names = self_names(name, is_character_legend, Some(rules_text));
    sorted_names.sort_by_key(|n| std::cmp::Reverse(n.encode_utf16().count()));

    for (raw_index, raw) in rules_text.split('\n').enumerate() {
        let stripped = strip_reminder_text_cow(raw);
        let text = replace_self_references_with_names(&stripped, name, &sorted_names);
        if text.is_empty() {
            continue;
        }
        if !parts.is_empty() {
            offset += 1; // '\n'
        }
        let span = Span {
            start: offset,
            end: offset + text.len() as u32,
        };
        let (ability_word, body_offset) = match match_ability_word_prefix(&text) {
            Some((aw, bo)) => (Some(aw), bo),
            None => (None, 0),
        };
        let body = text[body_offset..].to_string();
        let body_span = Span {
            start: offset + body_offset as u32,
            end: span.end,
        };
        let sentences = split_sentences(&body, body_span.start);

        lines.push(NormalizedLine {
            raw_index,
            raw: raw.trim().to_string(),
            text: text.clone(),
            span,
            ability_word,
            body,
            body_span,
            sentences,
        });
        parts.push(text);
        offset += lines.last().unwrap().text.len() as u32;
    }

    NormalizedFace {
        text: parts.join("\n"),
        lines,
    }
}
