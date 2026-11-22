use mtg_ir::{
    CardLayout, CardTypeMask, Characteristics, ColorMask, ManaCost, PrintedPt, SubtypeId,
    SupertypeMask,
};
use serde::{Deserialize, Deserializer, Serialize};
use smallvec::SmallVec;

use crate::intern::Interner;

/// Exact 8 non-game layouts filtered by TS `scripts/magic-shared.ts:10-19`.
pub const NON_GAME_LAYOUTS: [&str; 8] = [
    "art_series",
    "double_faced_token",
    "emblem",
    "front_card",
    "planar",
    "scheme",
    "token",
    "vanguard",
];

#[inline]
pub fn is_non_game_layout(layout: Option<&str>) -> bool {
    layout.is_some_and(|l| NON_GAME_LAYOUTS.contains(&l))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawFaceInput {
    pub name: String,
    pub mana_cost: String,
    pub type_line: String,
    pub oracle_text: String,
    pub power: Option<String>,
    pub toughness: Option<String>,
    pub loyalty: Option<String>,
    pub defense: Option<String>,
    pub colors: Vec<String>,
    pub color_indicator: Option<Vec<String>>,
    pub keywords: Vec<String>,
    pub cmc: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawCardInput {
    pub oracle_id: String,
    pub name: String,
    pub layout: Option<String>,
    pub mana_cost: String,
    pub type_line: String,
    pub oracle_text: String,
    pub power: Option<String>,
    pub toughness: Option<String>,
    pub loyalty: Option<String>,
    pub defense: Option<String>,
    pub colors: Vec<String>,
    pub color_indicator: Option<Vec<String>>,
    pub faces: Vec<RawFaceInput>,
    pub back_face: Option<RawFaceInput>,
}

fn deserialize_opt_string_or_num<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNum {
        Str(String),
        Int(i64),
        Float(f64),
    }
    let opt = Option::<StringOrNum>::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        StringOrNum::Str(s) => s,
        StringOrNum::Int(n) => n.to_string(),
        StringOrNum::Float(f) => f.to_string(),
    }))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScryfallJsonFace {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mana_cost: Option<String>,
    #[serde(default)]
    pub type_line: Option<String>,
    #[serde(default)]
    pub oracle_text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub power: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub toughness: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub loyalty: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub defense: Option<String>,
    #[serde(default)]
    pub colors: Option<Vec<String>>,
    #[serde(default)]
    pub color_indicator: Option<Vec<String>>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub cmc: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScryfallJsonCard {
    #[serde(default)]
    pub oracle_id: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub layout: Option<String>,
    #[serde(default)]
    pub mana_cost: Option<String>,
    #[serde(default)]
    pub type_line: Option<String>,
    #[serde(default)]
    pub oracle_text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub power: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub toughness: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub loyalty: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_num")]
    pub defense: Option<String>,
    #[serde(default)]
    pub colors: Option<Vec<String>>,
    #[serde(default)]
    pub color_indicator: Option<Vec<String>>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub cmc: Option<f64>,
    #[serde(default)]
    pub card_faces: Option<Vec<ScryfallJsonFace>>,
}

/// Clamps raw Scryfall layout strings to the 10 supported `CardLayout`s.
pub fn parse_card_layout(raw_layout: Option<&str>) -> Option<CardLayout> {
    match raw_layout? {
        "normal" => Some(CardLayout::Normal),
        "split" => Some(CardLayout::Split),
        "flip" => Some(CardLayout::Flip),
        "transform" => Some(CardLayout::Transform),
        "modal_dfc" => Some(CardLayout::ModalDfc),
        "adventure" => Some(CardLayout::Adventure),
        "meld" => Some(CardLayout::Meld),
        "reversible_card" => Some(CardLayout::ReversibleCard),
        "prepare" => Some(CardLayout::Prepare),
        "omen" => Some(CardLayout::Omen),
        _ => None,
    }
}

pub fn layout_to_str(layout: CardLayout) -> &'static str {
    match layout {
        CardLayout::Normal => "normal",
        CardLayout::Split => "split",
        CardLayout::Flip => "flip",
        CardLayout::Transform => "transform",
        CardLayout::ModalDfc => "modal_dfc",
        CardLayout::Adventure => "adventure",
        CardLayout::Meld => "meld",
        CardLayout::ReversibleCard => "reversible_card",
        CardLayout::Prepare => "prepare",
        CardLayout::Omen => "omen",
    }
}

/// Resolves the effective `CardLayout` for a [`RawCardInput`].
pub fn effective_layout(card: &RawCardInput) -> CardLayout {
    parse_card_layout(card.layout.as_deref()).unwrap_or_else(|| {
        if card.back_face.is_some() {
            CardLayout::Transform
        } else if card.faces.len() >= 2 {
            CardLayout::Split
        } else {
            CardLayout::Normal
        }
    })
}

/// Derives WUBRG color abbreviations from `{...}` mana symbols in WUBRG appearance order.
pub fn derive_colors_from_mana_cost(mana_cost: &str) -> Vec<String> {
    let mut colors: Vec<String> = Vec::new();
    let bytes = mana_cost.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(rel_end) = mana_cost[i + 1..].find('}')
        {
            let inner = &mana_cost[i + 1..i + 1 + rel_end];
            for c in ["W", "U", "B", "R", "G"] {
                let needle = c.as_bytes()[0];
                let has = inner.bytes().any(|b| b.to_ascii_uppercase() == needle);
                if has && !colors.iter().any(|existing| existing == c) {
                    colors.push(c.to_string());
                }
            }
            i = i + 2 + rel_end;
            continue;
        }
        i += 1;
    }
    colors
}

fn non_empty_str(opt: Option<&str>) -> Option<&str> {
    opt.filter(|s| !s.is_empty())
}

fn lowercase_alnum_underscore(s: &str) -> String {
    s.chars()
        .map(|ch| {
            let low = ch.to_ascii_lowercase();
            if low.is_ascii_alphanumeric() {
                low
            } else {
                '_'
            }
        })
        .collect()
}

/// Normalizes a raw Scryfall JSON card object into [`RawCardInput`], matching TS `scryfall.ts`.
pub fn normalize_scryfall_card(raw: &ScryfallJsonCard) -> RawCardInput {
    let raw_faces = raw.card_faces.as_deref().unwrap_or(&[]);
    let front = raw_faces.first();
    let back = raw_faces.get(1);

    let name = front
        .and_then(|f| non_empty_str(f.name.as_deref()))
        .or_else(|| non_empty_str(raw.name.as_deref()))
        .unwrap_or("Unknown Card")
        .to_string();

    let type_line = non_empty_str(raw.type_line.as_deref())
        .or_else(|| front.and_then(|f| non_empty_str(f.type_line.as_deref())))
        .unwrap_or("")
        .to_string();

    let oracle_text = if let Some(text) = &raw.oracle_text {
        text.clone()
    } else if !raw_faces.is_empty() {
        raw_faces
            .iter()
            .map(|f| f.oracle_text.as_deref().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n//\n")
    } else {
        String::new()
    };

    let mana_cost = raw
        .mana_cost
        .clone()
        .or_else(|| front.and_then(|f| f.mana_cost.clone()))
        .unwrap_or_default();

    let colors = raw
        .colors
        .clone()
        .or_else(|| front.and_then(|f| f.colors.clone()))
        .unwrap_or_default();

    let color_indicator = raw.color_indicator.clone();

    let power = front
        .and_then(|f| f.power.clone())
        .or_else(|| raw.power.clone());
    let toughness = front
        .and_then(|f| f.toughness.clone())
        .or_else(|| raw.toughness.clone());
    let loyalty = front
        .and_then(|f| f.loyalty.clone())
        .or_else(|| raw.loyalty.clone());
    let defense = front
        .and_then(|f| f.defense.clone())
        .or_else(|| raw.defense.clone());

    let oracle_id = non_empty_str(raw.oracle_id.as_deref())
        .or_else(|| non_empty_str(raw.id.as_deref()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("scry_{}", lowercase_alnum_underscore(&name)));

    let layout = parse_card_layout(raw.layout.as_deref())
        .map(layout_to_str)
        .map(str::to_string);

    let faces: Vec<RawFaceInput> = raw_faces
        .iter()
        .map(|f| {
            let f_name = non_empty_str(f.name.as_deref())
                .or_else(|| non_empty_str(raw.name.as_deref()))
                .unwrap_or("Unknown Card")
                .to_string();
            let f_mana_cost = f.mana_cost.clone().unwrap_or_default();
            // 3-tier face colors fallback matching TS scryfall.ts:213-217
            let f_colors = if let Some(c) = &f.colors {
                c.clone()
            } else if let Some(ind) = &f.color_indicator {
                ind.clone()
            } else {
                derive_colors_from_mana_cost(&f_mana_cost)
            };
            RawFaceInput {
                name: f_name,
                mana_cost: f_mana_cost,
                type_line: f.type_line.clone().unwrap_or_default(),
                oracle_text: f.oracle_text.clone().unwrap_or_default(),
                power: f.power.clone(),
                toughness: f.toughness.clone(),
                loyalty: f.loyalty.clone(),
                defense: f.defense.clone(),
                colors: f_colors,
                color_indicator: f.color_indicator.clone(),
                keywords: f.keywords.clone().unwrap_or_default(),
                cmc: f.cmc,
            }
        })
        .collect();

    let back_face = back.map(|b| {
        let b_mana_cost = b.mana_cost.clone().unwrap_or_default();
        let b_colors = derive_colors_from_mana_cost(&b_mana_cost);
        RawFaceInput {
            name: b.name.clone().unwrap_or_default(),
            mana_cost: b_mana_cost,
            type_line: b.type_line.clone().unwrap_or_default(),
            oracle_text: b.oracle_text.clone().unwrap_or_default(),
            power: b.power.clone(),
            toughness: b.toughness.clone(),
            loyalty: b.loyalty.clone(),
            defense: b.defense.clone(),
            colors: b_colors,
            color_indicator: None,
            keywords: Vec::new(),
            cmc: None,
        }
    });

    RawCardInput {
        oracle_id,
        name,
        layout,
        mana_cost,
        type_line,
        oracle_text,
        power,
        toughness,
        loyalty,
        defense,
        colors,
        color_indicator,
        faces,
        back_face,
    }
}

/// Returns the ordered list of faces for compilation, matching TS `getCardFaces`.
pub fn get_card_faces(card: &RawCardInput) -> Vec<RawFaceInput> {
    if !card.faces.is_empty() {
        return card.faces.clone();
    }
    if let Some(back) = &card.back_face {
        let front_oracle = card
            .oracle_text
            .split("\n//\n")
            .next()
            .unwrap_or(&card.oracle_text)
            .to_string();
        let front_type = card
            .type_line
            .split(" // ")
            .next()
            .unwrap_or(&card.type_line)
            .to_string();
        return vec![
            RawFaceInput {
                name: card.name.clone(),
                mana_cost: card.mana_cost.clone(),
                type_line: front_type,
                oracle_text: front_oracle,
                power: card.power.clone(),
                toughness: card.toughness.clone(),
                loyalty: card.loyalty.clone(),
                defense: card.defense.clone(),
                colors: card.colors.clone(),
                color_indicator: card.color_indicator.clone(),
                keywords: Vec::new(),
                cmc: None,
            },
            RawFaceInput {
                name: back.name.clone(),
                mana_cost: back.mana_cost.clone(),
                type_line: back.type_line.clone(),
                oracle_text: back.oracle_text.clone(),
                power: back.power.clone(),
                toughness: back.toughness.clone(),
                loyalty: back.loyalty.clone(),
                defense: back.defense.clone(),
                colors: derive_colors_from_mana_cost(&back.mana_cost),
                color_indicator: back.color_indicator.clone(),
                keywords: back.keywords.clone(),
                cmc: back.cmc,
            },
        ];
    }
    vec![RawFaceInput {
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
    }]
}

fn split_type_line(type_line: &str) -> (&str, &str) {
    let bytes = type_line.as_bytes();
    let dash = "\u{2014}";
    let mut idx = 0;
    while let Some(rel) = type_line[idx..].find(dash) {
        let pos = idx + rel;
        let has_left_ws = type_line[..pos]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let after_pos = pos + dash.len();
        let has_right_ws = type_line[after_pos..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        if has_left_ws && has_right_ws {
            let left = type_line[..pos].trim_end();
            let right = type_line[after_pos..].trim_start();
            return (left, right);
        }
        idx = pos + dash.len().min(bytes.len() - pos);
    }
    (type_line, "")
}

fn is_ts_supertype_token(word: &str) -> bool {
    matches!(
        word,
        "Basic" | "Legendary" | "Snow" | "World" | "Ongoing" | "Elite" | "Host" | "Token"
    )
}

pub fn parse_color_mask(colors: &[String]) -> ColorMask {
    let mut mask = ColorMask::empty();
    for c in colors {
        match c.to_ascii_uppercase().as_str() {
            "W" => mask |= ColorMask::W,
            "U" => mask |= ColorMask::U,
            "B" => mask |= ColorMask::B,
            "R" => mask |= ColorMask::R,
            "G" => mask |= ColorMask::G,
            "C" => mask |= ColorMask::C,
            _ => {}
        }
    }
    mask
}

fn parse_stat(opt: Option<&str>) -> Option<i32> {
    let s = opt?.trim();
    if s.is_empty() {
        return None;
    }
    let digits = s.strip_prefix('-').unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i32>().ok()
}

/// Builds typed [`Characteristics`] from a [`RawFaceInput`].
pub fn build_characteristics(face: &RawFaceInput, interner: &mut Interner) -> Characteristics {
    let (left, right) = split_type_line(&face.type_line);
    let mut types = CardTypeMask::empty();
    let mut supertypes = SupertypeMask::empty();
    for word in left.split_whitespace() {
        if is_ts_supertype_token(word) {
            supertypes |= SupertypeMask::parse_name(word);
        } else {
            types |= CardTypeMask::parse_name(word);
        }
    }

    let mut subtypes: SmallVec<[SubtypeId; 3]> = SmallVec::new();
    for st in right.split_whitespace() {
        if !st.is_empty() {
            subtypes.push(interner.intern_subtype(st));
        }
    }

    let colors = parse_color_mask(&face.colors);
    let color_indicator = face
        .color_indicator
        .as_deref()
        .filter(|v| !v.is_empty())
        .map(parse_color_mask);

    let power = parse_stat(face.power.as_deref());
    let toughness = parse_stat(face.toughness.as_deref());
    let pt_defined_by_ability = (face.power.is_some() && power.is_none())
        || (face.toughness.is_some() && toughness.is_none());

    let pt = if pt_defined_by_ability {
        Some(PrintedPt::Cda { power, toughness })
    } else if let (Some(p), Some(t)) = (power, toughness) {
        Some(PrintedPt::Fixed {
            power: p,
            toughness: t,
        })
    } else if power.is_some() || toughness.is_some() {
        Some(PrintedPt::Cda { power, toughness })
    } else {
        None
    };

    Characteristics {
        mana_cost: ManaCost::parse(&face.mana_cost),
        types,
        subtypes,
        supertypes,
        colors,
        color_indicator,
        pt,
        loyalty: parse_stat(face.loyalty.as_deref()),
        defense: parse_stat(face.defense.as_deref()),
    }
}

#[inline]
pub fn is_character_legend(ch: &Characteristics) -> bool {
    ch.supertypes.contains(SupertypeMask::LEGENDARY)
        && (ch.types.contains(CardTypeMask::CREATURE)
            || ch.types.contains(CardTypeMask::PLANESWALKER))
}

#[inline]
pub fn is_spell_characteristics(ch: &Characteristics) -> bool {
    ch.types
        .intersects(CardTypeMask::INSTANT | CardTypeMask::SORCERY)
}
