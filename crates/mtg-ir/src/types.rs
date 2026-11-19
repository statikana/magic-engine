use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Schema version for `mtg-ir` v2 binary and JSON interchange artifacts.
pub const IR_V2_VERSION: u32 = 2;

/// Dense identifier for a compiled [`crate::CardDef`] inside a [`crate::CardDatabase`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CardDefId(pub u32);

/// Interned card subtype identifier (e.g., `Elf`, `Island`, `Equipment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SubtypeId(pub u16);

/// Interned counter kind identifier (e.g., `+1/+1`, `-1/-1`, `loyalty`, `stun`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CounterId(pub u16);

/// Interned lower-case keyword identifier (e.g., `flying`, `ward`, `equip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeywordId(pub u16);

/// Character offsets into a face's normalized rules text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    W,
    U,
    B,
    R,
    G,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ManaColor {
    W,
    U,
    B,
    R,
    G,
    C,
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ColorMask: u8 {
        const W = 1 << 0;
        const U = 1 << 1;
        const B = 1 << 2;
        const R = 1 << 3;
        const G = 1 << 4;
        const C = 1 << 5;
        const ALL_COLORS = Self::W.bits() | Self::U.bits() | Self::B.bits() | Self::R.bits() | Self::G.bits();
    }
}

impl ColorMask {
    #[inline]
    pub fn from_mana_color(c: ManaColor) -> Self {
        match c {
            ManaColor::W => Self::W,
            ManaColor::U => Self::U,
            ManaColor::B => Self::B,
            ManaColor::R => Self::R,
            ManaColor::G => Self::G,
            ManaColor::C => Self::C,
        }
    }

    /// Number of MTG colors (`WUBRG`, excluding colorless `C`).
    #[inline]
    pub fn color_count(self) -> u32 {
        (self & Self::ALL_COLORS).bits().count_ones()
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct CardTypeMask: u16 {
        const LAND         = 1 << 0;
        const CREATURE     = 1 << 1;
        const ARTIFACT     = 1 << 2;
        const ENCHANTMENT  = 1 << 3;
        const PLANESWALKER = 1 << 4;
        const INSTANT      = 1 << 5;
        const SORCERY      = 1 << 6;
        const BATTLE       = 1 << 7;
        const KINDRED      = 1 << 8;
        const DUNGEON      = 1 << 9;
        const EMBLEM       = 1 << 10;
        const PERMANENT    = Self::LAND.bits() | Self::CREATURE.bits() | Self::ARTIFACT.bits()
                           | Self::ENCHANTMENT.bits() | Self::PLANESWALKER.bits() | Self::BATTLE.bits();
    }
}

impl CardTypeMask {
    pub fn parse_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "land" => Self::LAND,
            "creature" => Self::CREATURE,
            "artifact" => Self::ARTIFACT,
            "enchantment" => Self::ENCHANTMENT,
            "planeswalker" => Self::PLANESWALKER,
            "instant" => Self::INSTANT,
            "sorcery" => Self::SORCERY,
            "battle" => Self::BATTLE,
            "kindred" | "tribal" => Self::KINDRED,
            "dungeon" => Self::DUNGEON,
            "emblem" => Self::EMBLEM,
            "permanent" => Self::PERMANENT,
            _ => Self::empty(),
        }
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct SupertypeMask: u8 {
        const BASIC     = 1 << 0;
        const LEGENDARY = 1 << 1;
        const SNOW      = 1 << 2;
        const WORLD     = 1 << 3;
        const ONGOING   = 1 << 4;
    }
}

impl SupertypeMask {
    pub fn parse_name(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "basic" => Self::BASIC,
            "legendary" => Self::LEGENDARY,
            "snow" => Self::SNOW,
            "world" => Self::WORLD,
            "ongoing" => Self::ONGOING,
            _ => Self::empty(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Zone {
    Library,
    Hand,
    Battlefield,
    Graveyard,
    Exile,
    Command,
    Stack,
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct ZoneMask: u8 {
        const LIBRARY     = 1 << 0;
        const HAND        = 1 << 1;
        const BATTLEFIELD = 1 << 2;
        const GRAVEYARD   = 1 << 3;
        const EXILE       = 1 << 4;
        const COMMAND     = 1 << 5;
        const STACK       = 1 << 6;
        const ALL         = 0b0111_1111;
    }
}

impl ZoneMask {
    #[inline]
    pub fn from_zone(z: Zone) -> Self {
        match z {
            Zone::Library => Self::LIBRARY,
            Zone::Hand => Self::HAND,
            Zone::Battlefield => Self::BATTLEFIELD,
            Zone::Graveyard => Self::GRAVEYARD,
            Zone::Exile => Self::EXILE,
            Zone::Command => Self::COMMAND,
            Zone::Stack => Self::STACK,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GamePhase {
    Untap,
    Upkeep,
    Draw,
    Main1,
    BeginCombat,
    DeclareAttackers,
    DeclareBlockers,
    FirstStrikeDamage,
    CombatDamage,
    EndCombat,
    Main2,
    EndStep,
    Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardLayout {
    #[default]
    Normal,
    Split,
    Flip,
    Transform,
    ModalDfc,
    Adventure,
    Meld,
    ReversibleCard,
    Prepare,
    Omen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Timing {
    Instant,
    Sorcery,
    Mana,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Duration {
    EndOfTurn,
    Permanent,
    UntilYourNextTurn,
    UntilEndOfYourNextTurn,
    UntilSourceLeaves,
    UntilEndOfCombat,
    ForAsLongAsYouControlSelf,
    ForAsLongAsSelfRemainsTapped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerScope {
    ControllerTurn,
    EachTurn,
    OpponentTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StackTargetKind {
    Spell,
    Ability,
    ActivatedAbility,
    TriggeredAbility,
    SpellOrAbility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StatKind {
    Power,
    Toughness,
    ManaValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RoundMode {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompareOp {
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = "=")]
    Eq,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "<")]
    Lt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayerCounterKind {
    Poison,
    Experience,
    Energy,
    Rad,
    Ticket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CounterTarget {
    Any,
    Specific(CounterId),
}

/// Printed power/toughness on a card face (`None` for non-creatures without printed P/T).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PrintedPt {
    Fixed {
        power: i32,
        toughness: i32,
    },
    /// Characteristic-defining ability defines P and/or T (`*/*`, `*/N`, `N/*`).
    Cda {
        power: Option<i32>,
        toughness: Option<i32>,
    },
}

/// A single non-generic mana symbol in a parsed [`ManaCost`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ManaPip {
    Colored { color: ManaColor },
    Snow,
    Hybrid { a: ManaColor, b: ManaColor },
    TwoBrid { color: ManaColor },
    Phyrexian { color: ManaColor },
    HybridPhyrexian { a: ManaColor, b: ManaColor },
    ColorlessHybrid { color: ManaColor },
}

/// Pre-parsed MTG mana cost (`{2}{U}{U}`, `{X}{R}{G/P}`, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManaCost {
    #[serde(default)]
    pub generic: u16,
    #[serde(default)]
    pub x_count: u8,
    #[serde(default)]
    pub pips: SmallVec<[ManaPip; 4]>,
    /// Preserved raw notation when non-empty (e.g., for display or unrecognized symbols).
    #[serde(default)]
    pub raw: String,
}

impl ManaCost {
    /// Parses a Scryfall-style mana cost string like `"{2}{U}{U}"`, `"{X}{W/U}{B/P}"`.
    pub fn parse(raw: &str) -> Self {
        let mut out = Self {
            generic: 0,
            x_count: 0,
            pips: SmallVec::new(),
            raw: raw.to_string(),
        };
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{'
                && let Some(rel_end) = raw[i + 1..].find('}')
            {
                let sym = &raw[i + 1..i + 1 + rel_end];
                out.push_symbol(sym);
                i = i + 2 + rel_end;
                continue;
            }
            i += 1;
        }
        out
    }

    fn push_symbol(&mut self, sym: &str) {
        let up = sym.to_ascii_uppercase();
        if let Ok(n) = up.parse::<u16>() {
            self.generic = self.generic.saturating_add(n);
            return;
        }
        match up.as_str() {
            "X" | "Y" | "Z" => self.x_count = self.x_count.saturating_add(1),
            "W" => self.pips.push(ManaPip::Colored {
                color: ManaColor::W,
            }),
            "U" => self.pips.push(ManaPip::Colored {
                color: ManaColor::U,
            }),
            "B" => self.pips.push(ManaPip::Colored {
                color: ManaColor::B,
            }),
            "R" => self.pips.push(ManaPip::Colored {
                color: ManaColor::R,
            }),
            "G" => self.pips.push(ManaPip::Colored {
                color: ManaColor::G,
            }),
            "C" => self.pips.push(ManaPip::Colored {
                color: ManaColor::C,
            }),
            "S" => self.pips.push(ManaPip::Snow),
            _ => {
                let parts: SmallVec<[&str; 3]> = up.split('/').collect();
                if parts.len() == 2 {
                    match (parts[0], parts[1]) {
                        ("2", c) => {
                            if let Some(mc) = parse_color(c) {
                                self.pips.push(ManaPip::TwoBrid { color: mc });
                            }
                        }
                        ("C", c) => {
                            if let Some(mc) = parse_color(c) {
                                self.pips.push(ManaPip::ColorlessHybrid { color: mc });
                            }
                        }
                        (c, "P") => {
                            if let Some(mc) = parse_color(c) {
                                self.pips.push(ManaPip::Phyrexian { color: mc });
                            }
                        }
                        (a, b) => {
                            if let (Some(ca), Some(cb)) = (parse_color(a), parse_color(b)) {
                                self.pips.push(ManaPip::Hybrid { a: ca, b: cb });
                            }
                        }
                    }
                } else if parts.len() == 3
                    && parts[2] == "P"
                    && let (Some(ca), Some(cb)) = (parse_color(parts[0]), parse_color(parts[1]))
                {
                    self.pips.push(ManaPip::HybridPhyrexian { a: ca, b: cb });
                }
            }
        }
    }

    /// Mana value per CR 202.3 (X = 0 outside the stack; two-brid counts as 2).
    pub fn mana_value(&self) -> u32 {
        let mut mv = u32::from(self.generic);
        for pip in &self.pips {
            mv += match pip {
                ManaPip::TwoBrid { .. } => 2,
                _ => 1,
            };
        }
        mv
    }

    pub fn is_empty(&self) -> bool {
        self.generic == 0 && self.x_count == 0 && self.pips.is_empty() && self.raw.is_empty()
    }
}

fn parse_color(s: &str) -> Option<ManaColor> {
    match s {
        "W" => Some(ManaColor::W),
        "U" => Some(ManaColor::U),
        "B" => Some(ManaColor::B),
        "R" => Some(ManaColor::R),
        "G" => Some(ManaColor::G),
        "C" => Some(ManaColor::C),
        _ => None,
    }
}
