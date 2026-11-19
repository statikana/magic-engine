//! `mtg-ir`: Versioned Card IR v2, compact bitmasks, pre-parsed `ManaCost`, `CardDatabase`,
//! and binary `cards.bin` (`postcard` + optional `zstd`) serialization.
//!
//! Pure data crate: zero regexes, zero network/filesystem IO (byte-slice codecs only),
//! WASM-compatible with `--no-default-features`.

pub mod card;
pub mod cost;
pub mod effect;
pub mod filter;
pub mod replace;
pub mod static_def;
pub mod trigger;
pub mod types;
pub mod value;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

pub use card::*;
pub use cost::*;
pub use effect::*;
pub use filter::*;
pub use replace::*;
pub use static_def::*;
pub use trigger::*;
pub use types::*;
pub use value::*;

/// Binary header magic for `cards.bin` archives (`"MTGC"`).
pub const CARDS_BIN_MAGIC: [u8; 4] = *b"MTGC";
const COMPRESSION_NONE: u8 = 0;
const COMPRESSION_ZSTD: u8 = 1;

#[derive(Debug, Error)]
pub enum IrCodecError {
    #[error("invalid cards.bin magic header")]
    InvalidMagic,
    #[error("unsupported IR v2 binary version: {0} (expected {IR_V2_VERSION})")]
    UnsupportedVersion(u32),
    #[error("unsupported compression flag: {0}")]
    UnsupportedCompression(u8),
    #[error("zstd feature is required to read zstd-compressed cards.bin")]
    ZstdUnavailable,
    #[error("postcard codec error: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("io/compression error: {0}")]
    Io(#[from] std::io::Error),
}

/// String interner tables stored once inside [`CardDatabase`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternTables {
    pub subtypes: Vec<String>,
    pub counters: Vec<String>,
    pub keywords: Vec<String>,
}

/// Interchange bundle exported by `scripts/export-ir-v2.ts` (JSON) or compiled by `mtg-compiler`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrV2Bundle {
    pub ir_version: u32,
    pub source_compiler_version: u32,
    pub tables: InternTables,
    pub cards: Vec<CardDef>,
    #[serde(default)]
    pub decks: Vec<DeckDef>,
}

/// Runtime immutable card database shared via `Arc<CardDatabase>` across games and MCTS rollouts.
#[derive(Debug, Clone)]
pub struct CardDatabase {
    pub ir_version: u32,
    pub source_compiler_version: u32,
    pub tables: InternTables,
    pub cards: Vec<CardDef>,
    pub decks: Vec<DeckDef>,
    by_oracle_id: HashMap<String, CardDefId>,
    by_name: HashMap<String, CardDefId>,
    subtype_to_id: HashMap<String, SubtypeId>,
    counter_to_id: HashMap<String, CounterId>,
    keyword_to_id: HashMap<String, KeywordId>,
}

impl CardDatabase {
    pub fn from_bundle(bundle: IrV2Bundle) -> Result<Self, IrCodecError> {
        if bundle.ir_version != IR_V2_VERSION {
            return Err(IrCodecError::UnsupportedVersion(bundle.ir_version));
        }
        let mut by_oracle_id = HashMap::with_capacity(bundle.cards.len());
        let mut by_name = HashMap::with_capacity(bundle.cards.len());
        for (idx, card) in bundle.cards.iter().enumerate() {
            let id = CardDefId(idx as u32);
            by_oracle_id.insert(card.oracle_id.clone(), id);
            by_name.entry(card.name.clone()).or_insert(id);
        }
        let subtype_to_id = bundle
            .tables
            .subtypes
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), SubtypeId(i as u16)))
            .collect();
        let counter_to_id = bundle
            .tables
            .counters
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), CounterId(i as u16)))
            .collect();
        let keyword_to_id = bundle
            .tables
            .keywords
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), KeywordId(i as u16)))
            .collect();
        Ok(Self {
            ir_version: bundle.ir_version,
            source_compiler_version: bundle.source_compiler_version,
            tables: bundle.tables,
            cards: bundle.cards,
            decks: bundle.decks,
            by_oracle_id,
            by_name,
            subtype_to_id,
            counter_to_id,
            keyword_to_id,
        })
    }

    pub fn to_bundle(&self) -> IrV2Bundle {
        IrV2Bundle {
            ir_version: self.ir_version,
            source_compiler_version: self.source_compiler_version,
            tables: self.tables.clone(),
            cards: self.cards.clone(),
            decks: self.decks.clone(),
        }
    }

    #[inline]
    pub fn get(&self, id: CardDefId) -> Option<&CardDef> {
        self.cards.get(id.0 as usize)
    }

    #[inline]
    pub fn id_by_oracle(&self, oracle_id: &str) -> Option<CardDefId> {
        self.by_oracle_id.get(oracle_id).copied()
    }

    #[inline]
    pub fn id_by_name(&self, name: &str) -> Option<CardDefId> {
        self.by_name.get(name).copied()
    }

    #[inline]
    pub fn subtype_name(&self, id: SubtypeId) -> Option<&str> {
        self.tables.subtypes.get(id.0 as usize).map(String::as_str)
    }

    #[inline]
    pub fn subtype_id(&self, name: &str) -> Option<SubtypeId> {
        self.subtype_to_id.get(name).copied()
    }

    #[inline]
    pub fn counter_name(&self, id: CounterId) -> Option<&str> {
        self.tables.counters.get(id.0 as usize).map(String::as_str)
    }

    #[inline]
    pub fn counter_id(&self, name: &str) -> Option<CounterId> {
        self.counter_to_id.get(name).copied()
    }

    #[inline]
    pub fn keyword_name(&self, id: KeywordId) -> Option<&str> {
        self.tables.keywords.get(id.0 as usize).map(String::as_str)
    }

    #[inline]
    pub fn keyword_id(&self, name: &str) -> Option<KeywordId> {
        self.keyword_to_id.get(name).copied()
    }

    pub fn fully_supported_count(&self) -> usize {
        self.cards.iter().filter(|c| c.fully_supported).count()
    }

    /// Encode this database into a `cards.bin` byte buffer (uncompressed `postcard`, WASM-safe).
    pub fn encode_raw(&self) -> Result<Vec<u8>, IrCodecError> {
        let payload = postcard::to_allocvec(&self.to_bundle())?;
        let mut out = Vec::with_capacity(9 + payload.len());
        out.extend_from_slice(&CARDS_BIN_MAGIC);
        out.extend_from_slice(&IR_V2_VERSION.to_le_bytes());
        out.push(COMPRESSION_NONE);
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Encode this database into a zstd-compressed `cards.bin` byte buffer.
    #[cfg(feature = "zstd")]
    pub fn encode_zstd(&self, level: i32) -> Result<Vec<u8>, IrCodecError> {
        let payload = postcard::to_allocvec(&self.to_bundle())?;
        let compressed = zstd::bulk::compress(&payload, level)?;
        let mut out = Vec::with_capacity(9 + compressed.len());
        out.extend_from_slice(&CARDS_BIN_MAGIC);
        out.extend_from_slice(&IR_V2_VERSION.to_le_bytes());
        out.push(COMPRESSION_ZSTD);
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    /// Decode a `cards.bin` byte buffer (supports raw `postcard` everywhere, plus `zstd` when enabled).
    pub fn decode_bytes(bytes: &[u8]) -> Result<Self, IrCodecError> {
        if bytes.len() < 9 || bytes[0..4] != CARDS_BIN_MAGIC {
            return Err(IrCodecError::InvalidMagic);
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != IR_V2_VERSION {
            return Err(IrCodecError::UnsupportedVersion(version));
        }
        let compression = bytes[8];
        let body = &bytes[9..];
        let bundle: IrV2Bundle = match compression {
            COMPRESSION_NONE => postcard::from_bytes(body)?,
            COMPRESSION_ZSTD => {
                #[cfg(feature = "zstd")]
                {
                    let raw = zstd::stream::decode_all(body)?;
                    postcard::from_bytes(&raw)?
                }
                #[cfg(not(feature = "zstd"))]
                {
                    return Err(IrCodecError::ZstdUnavailable);
                }
            }
            other => return Err(IrCodecError::UnsupportedCompression(other)),
        };
        Self::from_bundle(bundle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mana_cost_parses_hybrid_phyrexian_and_mv() {
        let mc = ManaCost::parse("{2}{X}{U/R}{G/P}{2/W}");
        assert_eq!(mc.generic, 2);
        assert_eq!(mc.x_count, 1);
        assert_eq!(mc.pips.len(), 3);
        assert_eq!(mc.mana_value(), 2 + 1 + 1 + 2);
    }

    #[test]
    fn cards_bin_round_trip() {
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables {
                subtypes: vec!["Forest".into()],
                counters: vec!["+1/+1".into()],
                keywords: vec!["flying".into()],
            },
            cards: vec![CardDef {
                oracle_id: "card_forest".into(),
                compiler_version: 46,
                text_hash: "15dd7c47".into(),
                name: "Forest".into(),
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: "Forest".into(),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::LAND,
                        subtypes: smallvec::smallvec![SubtypeId(0)],
                        supertypes: SupertypeMask::BASIC,
                        colors: ColorMask::empty(),
                        color_indicator: None,
                        pt: None,
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![AbilityDef::Activated(ActivatedAbility {
                        id: "mana_tap".into(),
                        cost: vec![CostDef::TapSelf],
                        targets: vec![],
                        effect: vec![EffectStep {
                            op: EffectOp::AddMana {
                                production: ManaProduction::Fixed {
                                    symbols: smallvec::smallvec![ManaColor::G],
                                },
                                restriction: None,
                                restriction_spec: None,
                                retain_until_eot: false,
                            },
                            subject: Selector::None,
                            actor: None,
                            amount: None,
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
                        activation_restrictions: vec![],
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
                    })],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: true,
            }],
            decks: vec![],
        };
        let db = CardDatabase::from_bundle(bundle).unwrap();
        let raw = db.encode_raw().unwrap();
        let decoded = CardDatabase::decode_bytes(&raw).unwrap();
        assert_eq!(decoded.cards.len(), 1);
        assert_eq!(decoded.id_by_oracle("card_forest"), Some(CardDefId(0)));

        #[cfg(feature = "zstd")]
        {
            let zst = db.encode_zstd(3).unwrap();
            let decoded_zst = CardDatabase::decode_bytes(&zst).unwrap();
            assert_eq!(decoded_zst.cards.len(), 1);
            assert_eq!(decoded_zst.cards[0].name, "Forest");
        }
    }
}
