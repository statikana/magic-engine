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

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
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
const CHUNK_THRESHOLD_CARDS: usize = 4096;
const PARALLEL_CHUNKS: usize = 32;
#[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
const MAX_DECODE_WORKERS: usize = 8;
const MAX_CHUNK_RAW_BYTES: usize = 128 * 1024 * 1024;
const MAX_TOTAL_RAW_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct DecodeLimits {
    max_chunk_raw_bytes: usize,
    max_total_raw_bytes: usize,
}

const DEFAULT_DECODE_LIMITS: DecodeLimits = DecodeLimits {
    max_chunk_raw_bytes: MAX_CHUNK_RAW_BYTES,
    max_total_raw_bytes: MAX_TOTAL_RAW_BYTES,
};

#[inline]
fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Debug, Clone)]
struct StrIndex {
    slots: Vec<u32>,
    mask: usize,
}

impl StrIndex {
    fn build_with_hashes<'a, F>(
        len: usize,
        hashes: impl Iterator<Item = (usize, u64)>,
        mut key_at: F,
        overwrite_on_dup: bool,
    ) -> Self
    where
        F: FnMut(usize) -> &'a str,
    {
        let cap = (len.saturating_mul(2)).max(8).next_power_of_two();
        let mask = cap - 1;
        let mut slots = vec![u32::MAX; cap];
        for (idx, h) in hashes {
            let key = key_at(idx);
            let mut pos = (h as usize) & mask;
            loop {
                let cur = slots[pos];
                if cur == u32::MAX {
                    slots[pos] = idx as u32;
                    break;
                }
                if key_at(cur as usize) == key {
                    if overwrite_on_dup {
                        slots[pos] = idx as u32;
                    }
                    break;
                }
                pos = (pos + 1) & mask;
            }
        }
        Self { slots, mask }
    }

    #[inline]
    fn get_with<'a, F>(&self, key: &str, mut key_at: F) -> Option<u32>
    where
        F: FnMut(u32) -> &'a str,
    {
        let mut pos = (fnv1a_64(key) as usize) & self.mask;
        loop {
            let cur = self.slots[pos];
            if cur == u32::MAX {
                return None;
            }
            if key_at(cur) == key {
                return Some(cur);
            }
            pos = (pos + 1) & self.mask;
        }
    }
}

#[derive(Debug, Clone)]
struct StrIndex16 {
    slots: Vec<u16>,
    mask: usize,
}

impl StrIndex16 {
    fn build(entries: &[String]) -> Result<Self, IrCodecError> {
        if entries.len() >= u16::MAX as usize {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "intern table exceeds u16::MAX entries",
            )));
        }
        let cap = (entries.len().saturating_mul(2)).max(8).next_power_of_two();
        let mask = cap - 1;
        let mut slots = vec![u16::MAX; cap];
        for (idx, s) in entries.iter().enumerate() {
            let mut pos = (fnv1a_64(s) as usize) & mask;
            loop {
                let cur = slots[pos];
                if cur == u16::MAX {
                    slots[pos] = idx as u16;
                    break;
                }
                if entries[cur as usize] == *s {
                    slots[pos] = idx as u16;
                    break;
                }
                pos = (pos + 1) & mask;
            }
        }
        Ok(Self { slots, mask })
    }

    #[inline]
    fn get(&self, entries: &[String], key: &str) -> Option<u16> {
        let mut pos = (fnv1a_64(key) as usize) & self.mask;
        loop {
            let cur = self.slots[pos];
            if cur == u16::MAX {
                return None;
            }
            if entries[cur as usize] == key {
                return Some(cur);
            }
            pos = (pos + 1) & self.mask;
        }
    }
}

#[derive(Serialize)]
struct ChunkedHeader<'a> {
    ir_version: u32,
    source_compiler_version: u32,
    tables: &'a InternTables,
    decks: &'a [DeckDef],
    num_cards: u32,
}

#[derive(Deserialize)]
struct OwnedChunkedHeader {
    ir_version: u32,
    source_compiler_version: u32,
    tables: InternTables,
    decks: Vec<DeckDef>,
    num_cards: u32,
}

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
    by_oracle_id: StrIndex,
    by_name: StrIndex,
    subtype_to_id: StrIndex16,
    counter_to_id: StrIndex16,
    keyword_to_id: StrIndex16,
}

impl CardDatabase {
    pub fn from_bundle(bundle: IrV2Bundle) -> Result<Self, IrCodecError> {
        if bundle.ir_version != IR_V2_VERSION {
            return Err(IrCodecError::UnsupportedVersion(bundle.ir_version));
        }
        let hashes = compute_card_hashes(&bundle.cards)?;
        Self::from_parts_with_hashes(
            bundle.ir_version,
            bundle.source_compiler_version,
            bundle.tables,
            bundle.cards,
            bundle.decks,
            &hashes,
        )
    }

    fn from_parts_with_hashes(
        ir_version: u32,
        source_compiler_version: u32,
        tables: InternTables,
        cards: Vec<CardDef>,
        decks: Vec<DeckDef>,
        hashes: &[(u64, u64)],
    ) -> Result<Self, IrCodecError> {
        if cards.len() >= u32::MAX as usize {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "card database exceeds u32::MAX cards",
            )));
        }

        let (by_oracle_id, by_name) = build_card_indices_from_slices(&cards, &[hashes])?;
        let subtype_to_id = StrIndex16::build(&tables.subtypes)?;
        let counter_to_id = StrIndex16::build(&tables.counters)?;
        let keyword_to_id = StrIndex16::build(&tables.keywords)?;

        Ok(Self {
            ir_version,
            source_compiler_version,
            tables,
            cards,
            decks,
            by_oracle_id,
            by_name,
            subtype_to_id,
            counter_to_id,
            keyword_to_id,
        })
    }

    fn from_decoded_chunks(
        ir_version: u32,
        source_compiler_version: u32,
        tables: InternTables,
        mut decoded_chunks: Vec<DecodedChunk>,
        decks: Vec<DeckDef>,
        actual_len: usize,
    ) -> Result<Self, IrCodecError> {
        if actual_len >= u32::MAX as usize {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "card database exceeds u32::MAX cards",
            )));
        }

        let (cards, chunk_hashes): (Vec<CardDef>, Vec<Vec<(u64, u64)>>) =
            if decoded_chunks.is_empty() {
                (Vec::new(), Vec::new())
            } else if decoded_chunks.len() == 1 {
                let (c, h) = decoded_chunks.pop().unwrap_or_default();
                (c, vec![h])
            } else {
                let mut cards = Vec::with_capacity(actual_len);
                let mut chunk_hashes = Vec::with_capacity(decoded_chunks.len());
                for (chunk_cards, hashes) in decoded_chunks {
                    cards.extend(chunk_cards);
                    chunk_hashes.push(hashes);
                }
                (cards, chunk_hashes)
            };

        let hash_refs: Vec<&[(u64, u64)]> = chunk_hashes.iter().map(Vec::as_slice).collect();
        let (by_oracle_id, by_name) = build_card_indices_from_slices(&cards, &hash_refs)?;
        let subtype_to_id = StrIndex16::build(&tables.subtypes)?;
        let counter_to_id = StrIndex16::build(&tables.counters)?;
        let keyword_to_id = StrIndex16::build(&tables.keywords)?;

        Ok(Self {
            ir_version,
            source_compiler_version,
            tables,
            cards,
            decks,
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
        self.by_oracle_id
            .get_with(oracle_id, |i| self.cards[i as usize].oracle_id.as_str())
            .map(CardDefId)
    }

    #[inline]
    pub fn id_by_name(&self, name: &str) -> Option<CardDefId> {
        self.by_name
            .get_with(name, |i| self.cards[i as usize].name.as_str())
            .map(CardDefId)
    }

    #[inline]
    pub fn subtype_name(&self, id: SubtypeId) -> Option<&str> {
        self.tables.subtypes.get(id.0 as usize).map(String::as_str)
    }

    #[inline]
    pub fn subtype_id(&self, name: &str) -> Option<SubtypeId> {
        self.subtype_to_id
            .get(&self.tables.subtypes, name)
            .map(SubtypeId)
    }

    #[inline]
    pub fn counter_name(&self, id: CounterId) -> Option<&str> {
        self.tables.counters.get(id.0 as usize).map(String::as_str)
    }

    #[inline]
    pub fn counter_id(&self, name: &str) -> Option<CounterId> {
        self.counter_to_id
            .get(&self.tables.counters, name)
            .map(CounterId)
    }

    #[inline]
    pub fn keyword_name(&self, id: KeywordId) -> Option<&str> {
        self.tables.keywords.get(id.0 as usize).map(String::as_str)
    }

    #[inline]
    pub fn keyword_id(&self, name: &str) -> Option<KeywordId> {
        self.keyword_to_id
            .get(&self.tables.keywords, name)
            .map(KeywordId)
    }

    pub fn fully_supported_count(&self) -> usize {
        self.cards.iter().filter(|c| c.fully_supported).count()
    }

    fn encode_with_compression(
        &self,
        compression: u8,
        #[cfg_attr(not(feature = "zstd"), allow(unused_variables))] zstd_level: i32,
    ) -> Result<Vec<u8>, IrCodecError> {
        if self.cards.len() >= u32::MAX as usize {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "card count exceeds u32::MAX",
            )));
        }
        let num_cards = self.cards.len() as u32;
        let hdr = ChunkedHeader {
            ir_version: self.ir_version,
            source_compiler_version: self.source_compiler_version,
            tables: &self.tables,
            decks: &self.decks,
            num_cards,
        };
        let hdr_bytes = postcard::to_allocvec(&hdr)?;
        let hdr_len = u32::try_from(hdr_bytes.len()).map_err(|_| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "header exceeds u32::MAX bytes",
            ))
        })?;

        let chunk_slices: Vec<&[CardDef]> = if self.cards.is_empty() {
            Vec::new()
        } else if self.cards.len() >= CHUNK_THRESHOLD_CARDS {
            let chunk_size = self.cards.len().div_ceil(PARALLEL_CHUNKS);
            self.cards.chunks(chunk_size).collect()
        } else {
            vec![self.cards.as_slice()]
        };

        let encode_one = |slice: &[CardDef]| -> Result<Vec<u8>, IrCodecError> {
            let raw = postcard::to_allocvec(slice)?;
            match compression {
                COMPRESSION_NONE => Ok(raw),
                COMPRESSION_ZSTD => {
                    #[cfg(feature = "zstd")]
                    {
                        Ok(zstd::bulk::compress(&raw, zstd_level)?)
                    }
                    #[cfg(not(feature = "zstd"))]
                    {
                        Err(IrCodecError::ZstdUnavailable)
                    }
                }
                other => Err(IrCodecError::UnsupportedCompression(other)),
            }
        };

        let encoded_chunks: Vec<Vec<u8>>;
        #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
        {
            if self.cards.len() >= CHUNK_THRESHOLD_CARDS && chunk_slices.len() > 1 {
                let num_workers = std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(4)
                    .clamp(1, MAX_DECODE_WORKERS)
                    .min(chunk_slices.len());
                if num_workers > 1 {
                    type EncodedChunkBatch = Vec<(usize, Vec<u8>)>;
                    let next_idx = AtomicUsize::new(0);
                    let worker_results: Vec<Result<EncodedChunkBatch, IrCodecError>> =
                        std::thread::scope(|s| {
                            let next_ref = &next_idx;
                            let slices_ref = &chunk_slices;
                            let handles: Vec<_> = (0..num_workers)
                                .map(|_| {
                                    s.spawn(move || {
                                        let mut out = Vec::new();
                                        loop {
                                            let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                                            if idx >= slices_ref.len() {
                                                break;
                                            }
                                            out.push((idx, encode_one(slices_ref[idx])?));
                                        }
                                        Ok(out)
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .map(|h| {
                                    h.join().unwrap_or_else(|_| {
                                        Err(IrCodecError::Io(std::io::Error::other(
                                            "card chunk encode thread panicked",
                                        )))
                                    })
                                })
                                .collect()
                        });
                    let mut ordered: Vec<Option<Vec<u8>>> = vec![None; chunk_slices.len()];
                    for res in worker_results {
                        for (idx, chunk) in res? {
                            ordered[idx] = Some(chunk);
                        }
                    }
                    let mut collected = Vec::with_capacity(ordered.len());
                    for item in ordered {
                        collected.push(item.ok_or_else(|| {
                            IrCodecError::Io(std::io::Error::other("missing encoded card chunk"))
                        })?);
                    }
                    encoded_chunks = collected;
                } else {
                    let mut collected = Vec::with_capacity(chunk_slices.len());
                    for slice in chunk_slices {
                        collected.push(encode_one(slice)?);
                    }
                    encoded_chunks = collected;
                }
            } else {
                let mut collected = Vec::with_capacity(chunk_slices.len());
                for slice in chunk_slices {
                    collected.push(encode_one(slice)?);
                }
                encoded_chunks = collected;
            }
        }
        #[cfg(not(all(feature = "zstd", not(target_arch = "wasm32"))))]
        {
            let mut collected = Vec::with_capacity(chunk_slices.len());
            for slice in chunk_slices {
                collected.push(encode_one(slice)?);
            }
            encoded_chunks = collected;
        }

        let chunks_total_bytes: usize = encoded_chunks.iter().map(Vec::len).sum();
        let meta_bytes = encoded_chunks.len().checked_mul(4).ok_or_else(|| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk table exceeds usize",
            ))
        })?;
        let total_len = 9usize
            .checked_add(4)
            .and_then(|n| n.checked_add(hdr_bytes.len()))
            .and_then(|n| n.checked_add(4))
            .and_then(|n| n.checked_add(meta_bytes))
            .and_then(|n| n.checked_add(chunks_total_bytes))
            .ok_or_else(|| {
                IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "encoded cards.bin exceeds usize",
                ))
            })?;
        let n_chunks_u32 = u32::try_from(encoded_chunks.len()).map_err(|_| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk count exceeds u32::MAX",
            ))
        })?;

        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&CARDS_BIN_MAGIC);
        out.extend_from_slice(&IR_V2_VERSION.to_le_bytes());
        out.push(compression);
        out.extend_from_slice(&hdr_len.to_le_bytes());
        out.extend_from_slice(&hdr_bytes);
        out.extend_from_slice(&n_chunks_u32.to_le_bytes());
        for c in &encoded_chunks {
            let len = u32::try_from(c.len()).map_err(|_| {
                IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "chunk exceeds u32::MAX bytes",
                ))
            })?;
            out.extend_from_slice(&len.to_le_bytes());
        }
        for c in &encoded_chunks {
            out.extend_from_slice(c);
        }
        Ok(out)
    }

    /// Encode this database into a `cards.bin` byte buffer (uncompressed `postcard`, WASM-safe).
    pub fn encode_raw(&self) -> Result<Vec<u8>, IrCodecError> {
        self.encode_with_compression(COMPRESSION_NONE, 0)
    }

    /// Encode this database into a zstd-compressed `cards.bin` byte buffer.
    #[cfg(feature = "zstd")]
    pub fn encode_zstd(&self, level: i32) -> Result<Vec<u8>, IrCodecError> {
        self.encode_with_compression(COMPRESSION_ZSTD, level)
    }

    /// Decode a `cards.bin` byte buffer (supports raw `postcard` everywhere, plus `zstd` when enabled).
    pub fn decode_bytes(bytes: &[u8]) -> Result<Self, IrCodecError> {
        Self::decode_bytes_with_limits(bytes, DEFAULT_DECODE_LIMITS)
    }

    fn decode_bytes_with_limits(bytes: &[u8], limits: DecodeLimits) -> Result<Self, IrCodecError> {
        if bytes.len() < 9 || bytes[0..4] != CARDS_BIN_MAGIC {
            return Err(IrCodecError::InvalidMagic);
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != IR_V2_VERSION {
            return Err(IrCodecError::UnsupportedVersion(version));
        }
        let compression = bytes[8];
        if compression != COMPRESSION_NONE && compression != COMPRESSION_ZSTD {
            return Err(IrCodecError::UnsupportedCompression(compression));
        }
        #[cfg(not(feature = "zstd"))]
        if compression == COMPRESSION_ZSTD {
            return Err(IrCodecError::ZstdUnavailable);
        }

        let body = &bytes[9..];
        let mut pos = 0usize;

        let hdr_len = usize::try_from(read_u32_le(body, &mut pos)?).map_err(|_| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "header length exceeds address space",
            ))
        })?;
        if body.len().saturating_sub(pos) < hdr_len {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated cards.bin header",
            )));
        }
        let (hdr, hdr_rem): (OwnedChunkedHeader, &[u8]) =
            postcard::take_from_bytes(&body[pos..pos + hdr_len])?;
        if !hdr_rem.is_empty() {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "trailing bytes in cards.bin header",
            )));
        }
        pos += hdr_len;
        if hdr.ir_version != IR_V2_VERSION {
            return Err(IrCodecError::UnsupportedVersion(hdr.ir_version));
        }

        let num_cards = usize::try_from(hdr.num_cards).map_err(|_| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "card count exceeds address space",
            ))
        })?;
        if num_cards >= u32::MAX as usize {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "card database exceeds u32::MAX cards",
            )));
        }

        let n_chunks = usize::try_from(read_u32_le(body, &mut pos)?).map_err(|_| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk count exceeds address space",
            ))
        })?;
        if (num_cards == 0) != (n_chunks == 0) {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "cards.bin header/chunk mismatch: num_cards={num_cards}, n_chunks={n_chunks}"
                ),
            )));
        }
        if n_chunks > num_cards {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "cards.bin chunk count ({n_chunks}) exceeds header num_cards ({num_cards})"
                ),
            )));
        }
        let meta_bytes = n_chunks.checked_mul(4).ok_or_else(|| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk table overflow",
            ))
        })?;
        if body.len().saturating_sub(pos) < meta_bytes {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated chunk table",
            )));
        }
        let table = &body[pos..pos + meta_bytes];
        pos += meta_bytes;

        let mut chunk_slices = Vec::with_capacity(n_chunks);
        let mut table_pos = 0usize;
        for _ in 0..n_chunks {
            let len = usize::try_from(read_u32_le(table, &mut table_pos)?).map_err(|_| {
                IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "chunk length exceeds address space",
                ))
            })?;
            if body.len().saturating_sub(pos) < len {
                return Err(IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated chunk payload",
                )));
            }
            chunk_slices.push(&body[pos..pos + len]);
            pos += len;
        }
        if pos != body.len() {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "trailing bytes in cards.bin payload",
            )));
        }

        let raw_bytes_used = AtomicUsize::new(0);
        let decoded_chunks: Vec<DecodedChunk>;
        #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
        {
            if num_cards >= CHUNK_THRESHOLD_CARDS && chunk_slices.len() > 1 {
                let num_workers = std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(4)
                    .clamp(1, MAX_DECODE_WORKERS)
                    .min(chunk_slices.len());
                if num_workers > 1 {
                    use std::sync::atomic::AtomicBool;
                    type DecodedChunkBatch = Vec<(usize, DecodedChunk)>;
                    let next_idx = AtomicUsize::new(0);
                    let aborted = AtomicBool::new(false);
                    let worker_results: Vec<Result<DecodedChunkBatch, IrCodecError>> =
                        std::thread::scope(|s| {
                            let next_ref = &next_idx;
                            let abort_ref = &aborted;
                            let raw_ref = &raw_bytes_used;
                            let slices_ref = &chunk_slices;
                            let handles: Vec<_> = (0..num_workers)
                                .map(|_| {
                                    s.spawn(move || {
                                        let mut zstd_scratch = None;
                                        let mut out = Vec::new();
                                        loop {
                                            if abort_ref.load(Ordering::Relaxed) {
                                                break;
                                            }
                                            let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                                            if idx >= slices_ref.len() {
                                                break;
                                            }
                                            match decode_chunk_with_scratch(
                                                slices_ref[idx],
                                                compression,
                                                raw_ref,
                                                limits,
                                                &mut zstd_scratch,
                                            ) {
                                                Ok(decoded) => out.push((idx, decoded)),
                                                Err(e) => {
                                                    abort_ref.store(true, Ordering::Relaxed);
                                                    return Err(e);
                                                }
                                            }
                                        }
                                        Ok(out)
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .map(|h| {
                                    h.join().unwrap_or_else(|_| {
                                        Err(IrCodecError::Io(std::io::Error::other(
                                            "card chunk decode thread panicked",
                                        )))
                                    })
                                })
                                .collect()
                        });
                    let mut ordered: Vec<Option<DecodedChunk>> =
                        (0..chunk_slices.len()).map(|_| None).collect();
                    for res in worker_results {
                        for (idx, chunk) in res? {
                            ordered[idx] = Some(chunk);
                        }
                    }
                    let mut collected = Vec::with_capacity(ordered.len());
                    for item in ordered {
                        collected.push(item.ok_or_else(|| {
                            IrCodecError::Io(std::io::Error::other("missing decoded card chunk"))
                        })?);
                    }
                    decoded_chunks = collected;
                } else {
                    decoded_chunks = decode_chunks_sequential(
                        &chunk_slices,
                        compression,
                        &raw_bytes_used,
                        limits,
                    )?;
                }
            } else {
                decoded_chunks =
                    decode_chunks_sequential(&chunk_slices, compression, &raw_bytes_used, limits)?;
            }
        }
        #[cfg(not(all(feature = "zstd", not(target_arch = "wasm32"))))]
        {
            decoded_chunks =
                decode_chunks_sequential(&chunk_slices, compression, &raw_bytes_used, limits)?;
        }

        let mut actual_len: usize = 0;
        for (chunk_cards, _) in &decoded_chunks {
            actual_len = actual_len.checked_add(chunk_cards.len()).ok_or_else(|| {
                IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "decoded card count overflow",
                ))
            })?;
        }
        if actual_len != num_cards {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("cards.bin card count mismatch: header={num_cards}, decoded={actual_len}"),
            )));
        }

        Self::from_decoded_chunks(
            hdr.ir_version,
            hdr.source_compiler_version,
            hdr.tables,
            decoded_chunks,
            hdr.decks,
            actual_len,
        )
    }
}

type DecodedChunk = (Vec<CardDef>, Vec<(u64, u64)>);

#[inline]
fn read_u32_le(buf: &[u8], pos: &mut usize) -> Result<u32, IrCodecError> {
    if buf.len().saturating_sub(*pos) < 4 {
        return Err(IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "unexpected EOF reading u32",
        )));
    }
    let v = u32::from_le_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Ok(v)
}

fn reserve_raw_bytes(
    used: &AtomicUsize,
    amount: usize,
    max_total_raw_bytes: usize,
) -> Result<(), IrCodecError> {
    used.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        let next = cur.checked_add(amount)?;
        (next <= max_total_raw_bytes).then_some(next)
    })
    .map(|_| ())
    .map_err(|_| {
        IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cards.bin total raw size exceeds limit ({max_total_raw_bytes} bytes)"),
        ))
    })
}

struct BoundedCardVecVisitor {
    max_cards: usize,
}

impl<'de> serde::de::Visitor<'de> for BoundedCardVecVisitor {
    type Value = Vec<CardDef>;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a sequence of CardDef")
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let cap = seq.size_hint().unwrap_or(0).min(self.max_cards).min(65_536);
        let mut cards = Vec::with_capacity(cap);
        while let Some(card) = seq.next_element()? {
            cards.push(card);
        }
        Ok(cards)
    }
}

fn deserialize_chunk_cards(raw: &[u8]) -> Result<Vec<CardDef>, IrCodecError> {
    use serde::Deserializer as _;
    let mut de = postcard::Deserializer::from_bytes(raw);
    let cards = de.deserialize_seq(BoundedCardVecVisitor {
        max_cards: raw.len() / 7,
    })?;
    let rem = de.finalize()?;
    if !rem.is_empty() {
        return Err(IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "trailing bytes in card chunk",
        )));
    }
    if cards.is_empty() {
        return Err(IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty card chunk",
        )));
    }
    Ok(cards)
}

#[cfg(feature = "zstd")]
struct ZstdScratch {
    decompressor: zstd::bulk::Decompressor<'static>,
    raw_buf: Vec<u8>,
}

#[cfg(feature = "zstd")]
impl ZstdScratch {
    fn new() -> Result<Self, IrCodecError> {
        Ok(Self {
            decompressor: zstd::bulk::Decompressor::new()?,
            raw_buf: Vec::new(),
        })
    }
}

#[cfg(feature = "zstd")]
fn stream_decompress_bounded(
    chunk: &[u8],
    raw_bytes_used: &AtomicUsize,
    limits: DecodeLimits,
    raw_buf: &mut Vec<u8>,
) -> Result<(), IrCodecError> {
    use std::io::Read as _;

    raw_buf.clear();
    let mut decoder = zstd::stream::Decoder::with_buffer(chunk)?;
    decoder.window_log_max(27)?;
    let mut limited = decoder.take((limits.max_chunk_raw_bytes as u64).saturating_add(1));
    let mut scratch = [0u8; 64 * 1024];
    loop {
        let n = match limited.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(IrCodecError::Io(e)),
        };
        let new_len = raw_buf.len().checked_add(n).ok_or_else(|| {
            IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "zstd chunk size overflow",
            ))
        })?;
        if new_len > limits.max_chunk_raw_bytes {
            return Err(IrCodecError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "streaming zstd chunk exceeds limit ({} bytes)",
                    limits.max_chunk_raw_bytes
                ),
            )));
        }
        reserve_raw_bytes(raw_bytes_used, n, limits.max_total_raw_bytes)?;
        raw_buf.extend_from_slice(&scratch[..n]);
    }
    if raw_buf.is_empty() {
        return Err(IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "streaming zstd chunk produced 0 raw bytes",
        )));
    }
    Ok(())
}

#[cfg(feature = "zstd")]
fn decode_zstd_chunk<'a>(
    chunk: &[u8],
    raw_bytes_used: &AtomicUsize,
    limits: DecodeLimits,
    scratch: &'a mut Option<ZstdScratch>,
) -> Result<&'a [u8], IrCodecError> {
    match zstd::zstd_safe::get_frame_content_size(chunk) {
        Ok(Some(0)) => Err(IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zstd chunk declares 0 raw bytes",
        ))),
        Ok(Some(size)) => {
            let Some(size_usize) = usize::try_from(size)
                .ok()
                .filter(|&s| s <= limits.max_chunk_raw_bytes)
            else {
                return Err(IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "zstd chunk declares {size} raw bytes, exceeding limit ({})",
                        limits.max_chunk_raw_bytes
                    ),
                )));
            };

            let sc = match scratch {
                Some(sc) => sc,
                None => scratch.insert(ZstdScratch::new()?),
            };

            if size_usize <= chunk.len().saturating_mul(1024).max(1024 * 1024) {
                reserve_raw_bytes(raw_bytes_used, size_usize, limits.max_total_raw_bytes)?;
                sc.raw_buf.clear();
                if sc.raw_buf.capacity() < size_usize {
                    let cap_limit = limits.max_chunk_raw_bytes.min(limits.max_total_raw_bytes);
                    let double_cap = sc.raw_buf.capacity().saturating_mul(2).min(cap_limit);
                    let target_cap = size_usize.max(double_cap);
                    sc.raw_buf.reserve_exact(target_cap);
                }
                let written = sc
                    .decompressor
                    .decompress_to_buffer(chunk, &mut sc.raw_buf)?;
                if written != size_usize {
                    return Err(IrCodecError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("zstd chunk decoded {written} bytes, expected {size_usize}"),
                    )));
                }
                Ok(&sc.raw_buf[..written])
            } else {
                stream_decompress_bounded(chunk, raw_bytes_used, limits, &mut sc.raw_buf)?;
                if sc.raw_buf.len() != size_usize {
                    return Err(IrCodecError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "zstd chunk decoded {} bytes, expected {size_usize}",
                            sc.raw_buf.len()
                        ),
                    )));
                }
                Ok(sc.raw_buf.as_slice())
            }
        }
        Ok(None) => {
            let sc = match scratch {
                Some(sc) => sc,
                None => scratch.insert(ZstdScratch::new()?),
            };
            stream_decompress_bounded(chunk, raw_bytes_used, limits, &mut sc.raw_buf)?;
            Ok(sc.raw_buf.as_slice())
        }
        Err(_) => Err(IrCodecError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid or truncated zstd frame header",
        ))),
    }
}

#[cfg(feature = "zstd")]
fn decode_chunk_with_scratch(
    chunk: &[u8],
    compression: u8,
    raw_bytes_used: &AtomicUsize,
    limits: DecodeLimits,
    zstd_scratch: &mut Option<ZstdScratch>,
) -> Result<DecodedChunk, IrCodecError> {
    let cards: Vec<CardDef> = match compression {
        COMPRESSION_NONE => {
            if chunk.len() > limits.max_chunk_raw_bytes {
                return Err(IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "raw chunk size {} exceeds limit ({})",
                        chunk.len(),
                        limits.max_chunk_raw_bytes
                    ),
                )));
            }
            reserve_raw_bytes(raw_bytes_used, chunk.len(), limits.max_total_raw_bytes)?;
            deserialize_chunk_cards(chunk)?
        }
        COMPRESSION_ZSTD => {
            let raw = decode_zstd_chunk(chunk, raw_bytes_used, limits, zstd_scratch)?;
            deserialize_chunk_cards(raw)?
        }
        other => return Err(IrCodecError::UnsupportedCompression(other)),
    };
    let hashes = cards
        .iter()
        .map(|c| (fnv1a_64(&c.oracle_id), fnv1a_64(&c.name)))
        .collect();
    Ok((cards, hashes))
}

#[cfg(not(feature = "zstd"))]
fn decode_chunk_raw(
    chunk: &[u8],
    compression: u8,
    raw_bytes_used: &AtomicUsize,
    limits: DecodeLimits,
) -> Result<DecodedChunk, IrCodecError> {
    let cards: Vec<CardDef> = match compression {
        COMPRESSION_NONE => {
            if chunk.len() > limits.max_chunk_raw_bytes {
                return Err(IrCodecError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "raw chunk size {} exceeds limit ({})",
                        chunk.len(),
                        limits.max_chunk_raw_bytes
                    ),
                )));
            }
            reserve_raw_bytes(raw_bytes_used, chunk.len(), limits.max_total_raw_bytes)?;
            deserialize_chunk_cards(chunk)?
        }
        COMPRESSION_ZSTD => return Err(IrCodecError::ZstdUnavailable),
        other => return Err(IrCodecError::UnsupportedCompression(other)),
    };
    let hashes = cards
        .iter()
        .map(|c| (fnv1a_64(&c.oracle_id), fnv1a_64(&c.name)))
        .collect();
    Ok((cards, hashes))
}

fn decode_chunks_sequential(
    chunk_slices: &[&[u8]],
    compression: u8,
    raw_bytes_used: &AtomicUsize,
    limits: DecodeLimits,
) -> Result<Vec<DecodedChunk>, IrCodecError> {
    let mut out = Vec::with_capacity(chunk_slices.len());
    #[cfg(feature = "zstd")]
    {
        let mut zstd_scratch = None;
        for &chunk in chunk_slices {
            out.push(decode_chunk_with_scratch(
                chunk,
                compression,
                raw_bytes_used,
                limits,
                &mut zstd_scratch,
            )?);
        }
    }
    #[cfg(not(feature = "zstd"))]
    {
        for &chunk in chunk_slices {
            out.push(decode_chunk_raw(
                chunk,
                compression,
                raw_bytes_used,
                limits,
            )?);
        }
    }
    Ok(out)
}

fn compute_card_hashes(cards: &[CardDef]) -> Result<Vec<(u64, u64)>, IrCodecError> {
    #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
    {
        if cards.len() >= CHUNK_THRESHOLD_CARDS {
            let num_workers = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
                .clamp(1, MAX_DECODE_WORKERS)
                .min(cards.len());
            if num_workers > 1 {
                let chunk_size = cards.len().div_ceil(num_workers);
                let chunks: Vec<Result<Vec<(u64, u64)>, IrCodecError>> = std::thread::scope(|s| {
                    let handles: Vec<_> = cards
                        .chunks(chunk_size)
                        .map(|slice| {
                            s.spawn(move || {
                                slice
                                    .iter()
                                    .map(|c| (fnv1a_64(&c.oracle_id), fnv1a_64(&c.name)))
                                    .collect::<Vec<_>>()
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().map_err(|_| {
                                IrCodecError::Io(std::io::Error::other("card hash thread panicked"))
                            })
                        })
                        .collect()
                });
                let mut out = Vec::with_capacity(cards.len());
                for ch in chunks {
                    out.extend(ch?);
                }
                return Ok(out);
            }
        }
    }
    Ok(cards
        .iter()
        .map(|c| (fnv1a_64(&c.oracle_id), fnv1a_64(&c.name)))
        .collect())
}

fn build_card_indices_from_slices(
    cards: &[CardDef],
    hash_slices: &[&[(u64, u64)]],
) -> Result<(StrIndex, StrIndex), IrCodecError> {
    #[cfg(all(feature = "zstd", not(target_arch = "wasm32")))]
    {
        if cards.len() >= CHUNK_THRESHOLD_CARDS {
            return std::thread::scope(|s| {
                let h_oracle = s.spawn(|| {
                    StrIndex::build_with_hashes(
                        cards.len(),
                        hash_slices
                            .iter()
                            .flat_map(|sl| sl.iter())
                            .enumerate()
                            .map(|(i, &(oh, _))| (i, oh)),
                        |i| cards[i].oracle_id.as_str(),
                        true,
                    )
                });
                let by_name = StrIndex::build_with_hashes(
                    cards.len(),
                    hash_slices
                        .iter()
                        .flat_map(|sl| sl.iter())
                        .enumerate()
                        .map(|(i, &(_, nh))| (i, nh)),
                    |i| cards[i].name.as_str(),
                    false,
                );
                let by_oracle_id = h_oracle.join().map_err(|_| {
                    IrCodecError::Io(std::io::Error::other("card index builder thread panicked"))
                })?;
                Ok((by_oracle_id, by_name))
            });
        }
    }
    let by_oracle_id = StrIndex::build_with_hashes(
        cards.len(),
        hash_slices
            .iter()
            .flat_map(|sl| sl.iter())
            .enumerate()
            .map(|(i, &(oh, _))| (i, oh)),
        |i| cards[i].oracle_id.as_str(),
        true,
    );
    let by_name = StrIndex::build_with_hashes(
        cards.len(),
        hash_slices
            .iter()
            .flat_map(|sl| sl.iter())
            .enumerate()
            .map(|(i, &(_, nh))| (i, nh)),
        |i| cards[i].name.as_str(),
        false,
    );
    Ok((by_oracle_id, by_name))
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

        let cp = ManaCost::parse("{C/P}");
        assert_eq!(
            cp.pips.as_slice(),
            &[ManaPip::Phyrexian {
                color: ManaColor::C
            }]
        );
        assert_eq!(cp.mana_value(), 1);

        let cw = ManaCost::parse("{C/W}");
        assert_eq!(
            cw.pips.as_slice(),
            &[ManaPip::ColorlessHybrid {
                color: ManaColor::W
            }]
        );
        assert_eq!(cw.mana_value(), 1);

        let gleemax = ManaCost::parse("{1000000}");
        assert_eq!(gleemax.generic, 1_000_000);
        assert_eq!(gleemax.mana_value(), 1_000_000);
    }

    #[test]
    fn bitmasks_round_trip_as_integers_in_json_and_postcard() {
        let color = ColorMask::W | ColorMask::G | ColorMask::C;
        let card_type = CardTypeMask::CREATURE | CardTypeMask::CONSPIRACY;
        let supertype = SupertypeMask::LEGENDARY | SupertypeMask::SNOW;
        let zone = ZoneMask::BATTLEFIELD | ZoneMask::EXILE;
        let flags = FilterFlags::OTHER
            | FilterFlags::UNBLOCKED_ATTACKER
            | FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN;

        assert_eq!(
            serde_json::to_string(&color).unwrap(),
            color.bits().to_string()
        );
        assert_eq!(
            serde_json::to_string(&card_type).unwrap(),
            card_type.bits().to_string()
        );
        assert_eq!(
            serde_json::to_string(&supertype).unwrap(),
            supertype.bits().to_string()
        );
        assert_eq!(
            serde_json::to_string(&zone).unwrap(),
            zone.bits().to_string()
        );
        assert_eq!(
            serde_json::to_string(&flags).unwrap(),
            flags.bits().to_string()
        );
        assert_eq!(
            FilterFlags::ARTIFACT_ENCHANTMENT_OR_TOKEN.bits(),
            1u64 << 52
        );

        assert_eq!(
            serde_json::from_str::<ColorMask>(&color.bits().to_string()).unwrap(),
            color
        );
        assert_eq!(
            serde_json::from_str::<CardTypeMask>(&card_type.bits().to_string()).unwrap(),
            card_type
        );
        assert_eq!(
            serde_json::from_str::<SupertypeMask>(&supertype.bits().to_string()).unwrap(),
            supertype
        );
        assert_eq!(
            serde_json::from_str::<ZoneMask>(&zone.bits().to_string()).unwrap(),
            zone
        );
        assert_eq!(
            serde_json::from_str::<FilterFlags>(&flags.bits().to_string()).unwrap(),
            flags
        );

        // String bitflags in JSON and out-of-range bits (bit 53) must fail.
        assert!(serde_json::from_str::<ColorMask>("\"W\"").is_err());
        assert!(serde_json::from_str::<FilterFlags>(&(1u64 << 53).to_string()).is_err());
        assert!(serde_json::from_str::<CardTypeMask>(&(1u16 << 15).to_string()).is_err());

        // Postcard round-trip preserves exact bits.
        let p_flags = postcard::to_allocvec(&flags).unwrap();
        assert_eq!(
            postcard::from_bytes::<FilterFlags>(&p_flags).unwrap(),
            flags
        );
        let p_ct = postcard::to_allocvec(&card_type).unwrap();
        assert_eq!(
            postcard::from_bytes::<CardTypeMask>(&p_ct).unwrap(),
            card_type
        );

        // parse_name recognizes Conspiracy.
        assert_eq!(
            CardTypeMask::parse_name("Conspiracy"),
            CardTypeMask::CONSPIRACY
        );
        assert!(!CardTypeMask::PERMANENT.contains(CardTypeMask::CONSPIRACY));
    }

    #[test]
    fn f1_schema_fixes_round_trip_json_and_postcard() {
        // ZoneChangeMod::opponent_discard_to_battlefield 3-way distinction
        let zc_none = ZoneChangeMod::default();
        let zc_plain = ZoneChangeMod {
            opponent_discard_to_battlefield: Some(OpponentDiscardToBattlefield::Plain),
            ..Default::default()
        };
        let zc_counters = ZoneChangeMod {
            opponent_discard_to_battlefield: Some(OpponentDiscardToBattlefield::WithCounters {
                counter: CounterId(0),
                count: 2,
            }),
            ..Default::default()
        };
        for zc in [&zc_none, &zc_plain, &zc_counters] {
            let j = serde_json::to_string(zc).unwrap();
            assert_eq!(&serde_json::from_str::<ZoneChangeMod>(&j).unwrap(), zc);
            let p = postcard::to_allocvec(zc).unwrap();
            assert_eq!(&postcard::from_bytes::<ZoneChangeMod>(&p).unwrap(), zc);
        }

        // DamageScaleSpec.target_filter defaults to Any when omitted
        let dmg: DamageScaleSpec = serde_json::from_str(r#"{"mode":"double"}"#).unwrap();
        assert_eq!(dmg.target_filter, DamageModTarget::Any);

        // CounterTarget::Each
        let ct: CounterTarget = serde_json::from_str(r#""each""#).unwrap();
        assert_eq!(ct, CounterTarget::Each);
        assert_eq!(
            postcard::from_bytes::<CounterTarget>(&postcard::to_allocvec(&ct).unwrap()).unwrap(),
            ct
        );

        // ObjectPredicate::NotSelf, PlayerOnly & RelativeControl Opponents/All
        let filt: ObjectFilter = serde_json::from_str(
            r#"{"controller":"opponents","owner":"all","extra":["notSelf","playerOnly"]}"#,
        )
        .unwrap();
        assert_eq!(filt.controller, Some(RelativeControl::Opponents));
        assert_eq!(filt.owner, Some(RelativeControl::All));
        assert_eq!(
            filt.extra,
            vec![ObjectPredicate::NotSelf, ObjectPredicate::PlayerOnly]
        );
        assert_eq!(
            postcard::from_bytes::<ObjectFilter>(&postcard::to_allocvec(&filt).unwrap()).unwrap(),
            filt
        );

        // SpellAbility.id & UnsupportedAbility.cost_keyword
        let spell: SpellAbility = serde_json::from_str(r#"{"id":"cast"}"#).unwrap();
        assert_eq!(spell.id.as_deref(), Some("cast"));
        let unsup: UnsupportedAbility =
            serde_json::from_str(r#"{"reason":"unparsed","text":"Boast","cost_keyword":3}"#)
                .unwrap();
        assert_eq!(unsup.cost_keyword, Some(KeywordId(3)));

        // StaticBody::Multiplier factor 0.5, AltCastPermission.use_printed_mana_cost, StaticOnPrevented.to_amount
        let mult: StaticDef = serde_json::from_str(
            r#"{"body":{"multiplier":{"what":"counters","factor":0.5,"round":"down","who_places":"opponents"}},"condition":null}"#,
        )
        .unwrap();
        let mult_p = postcard::to_allocvec(&mult).unwrap();
        assert_eq!(postcard::from_bytes::<StaticDef>(&mult_p).unwrap(), mult);

        let alt: StaticDef = serde_json::from_str(
            r#"{"body":{"altCastPermission":{"mana_cost":{},"use_printed_mana_cost":true,"affected":"selfObj"}}}"#,
        )
        .unwrap();
        assert_eq!(
            postcard::from_bytes::<StaticDef>(&postcard::to_allocvec(&alt).unwrap()).unwrap(),
            alt
        );

        let prev: StaticOnPrevented = serde_json::from_str(
            r#"{"reflexiveDamageTrigger":{"target":{"kind":"object","min":1,"max":1},"to_amount":"prevented"}}"#,
        )
        .unwrap();
        assert_eq!(
            postcard::from_bytes::<StaticOnPrevented>(&postcard::to_allocvec(&prev).unwrap())
                .unwrap(),
            prev
        );

        // DigSpec.rest_order
        let dig: DigSpec = serde_json::from_str(
            r#"{"keep":1,"keep_to":"hand","rest":"libraryBottom","rest_order":"any"}"#,
        )
        .unwrap();
        assert_eq!(dig.rest_order, Some(DigRestOrder::Any));
        assert_eq!(
            postcard::from_bytes::<DigSpec>(&postcard::to_allocvec(&dig).unwrap()).unwrap(),
            dig
        );

        // Value.rs: ResultCount{of: None}, DistinctManaValuesInGraveyard{filter}, DistinctNamesControlled{filter},
        // AttackingCount{filter}, OpponentHadPermEnteredThisTurn{min}
        let rc: Amount = serde_json::from_str(r#"{"resultCount":{}}"#).unwrap();
        assert_eq!(rc, Amount::ResultCount { of: None });
        let dmv: Amount = serde_json::from_str(
            r#"{"distinctManaValuesInGraveyard":{"filter":{"types_any":96}}}"#,
        )
        .unwrap();
        let dnc: Amount =
            serde_json::from_str(r#"{"distinctNamesControlled":{"filter":{"types_any":2}}}"#)
                .unwrap();
        for amt in [&rc, &dmv, &dnc] {
            assert_eq!(
                postcard::from_bytes::<Amount>(&postcard::to_allocvec(amt).unwrap()).unwrap(),
                *amt
            );
        }

        let atk: Condition = serde_json::from_str(
            r#"{"attackingCount":{"op":">=","n":2,"filter":{"types_any":2}}}"#,
        )
        .unwrap();
        let opp_perm_default: Condition =
            serde_json::from_str(r#"{"opponentHadPermEnteredThisTurn":{}}"#).unwrap();
        assert_eq!(
            opp_perm_default,
            Condition::OpponentHadPermEnteredThisTurn {
                filter: None,
                min: 1
            }
        );
        let opp_perm_two: Condition =
            serde_json::from_str(r#"{"opponentHadPermEnteredThisTurn":{"min":2}}"#).unwrap();
        for cond in [&atk, &opp_perm_default, &opp_perm_two] {
            assert_eq!(
                postcard::from_bytes::<Condition>(&postcard::to_allocvec(cond).unwrap()).unwrap(),
                *cond
            );
        }

        // Trigger.rs: EventDetail::Combat.host and EventDetail::LandPlayed
        let trig: EventFilter =
            serde_json::from_str(r#"{"detail":{"combat":{"host":{"types_any":2}}}}"#).unwrap();
        let trig_land: EventFilter = serde_json::from_str(
            r#"{"detail":{"landPlayed":{"cast_from_not_hand":true,"cast_from_zone":"exile"}}}"#,
        )
        .unwrap();
        for ef in [&trig, &trig_land] {
            assert_eq!(
                postcard::from_bytes::<EventFilter>(&postcard::to_allocvec(ef).unwrap()).unwrap(),
                *ef
            );
        }

        // Effect.rs: PileSplitSpec.other_to = libraryBottom and AddMana.delayed_rider
        let pile: PileSplitSpec = serde_json::from_str(
            r#"{"reveal":true,"pile_maker":"you","pile_chooser":"opponent","chosen_to":"hand","other_to":"libraryBottom","other_random_order":true}"#,
        )
        .unwrap();
        assert_eq!(pile.other_to, DigRestTo::LibraryBottom);
        assert_eq!(
            postcard::from_bytes::<PileSplitSpec>(&postcard::to_allocvec(&pile).unwrap()).unwrap(),
            pile
        );

        let add_mana: EffectOp = serde_json::from_str(
            r#"{"addMana":{"production":{"choice":{"kind":"any","colors":0}},"delayed_rider":{"trigger":{"event":"cast","filter":{}},"effect":[],"next_turn":false}}}"#,
        )
        .unwrap();
        assert_eq!(
            postcard::from_bytes::<EffectOp>(&postcard::to_allocvec(&add_mana).unwrap()).unwrap(),
            add_mana
        );
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
                                delayed_rider: None,
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
        let json = serde_json::to_string(&bundle).unwrap();
        let from_json: IrV2Bundle = serde_json::from_str(&json).unwrap();
        assert_eq!(from_json, bundle);

        let db = CardDatabase::from_bundle(from_json).unwrap();
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

    #[test]
    fn player_only_multiplier_round_trip() {
        let json = r#"{
            "body": {
                "multiplier": {
                    "what": "counters",
                    "factor": 1.0,
                    "delta": 1,
                    "applies_to_players": true,
                    "filter": {
                        "controller": "you",
                        "extra": ["playerOnly"]
                    }
                }
            },
            "condition": null
        }"#;
        let def: StaticDef = serde_json::from_str(json).unwrap();
        match &def.body {
            StaticBody::Multiplier {
                applies_to_players,
                filter,
                ..
            } => {
                assert!(*applies_to_players);
                let f = filter.as_ref().expect("filter must be preserved");
                assert_eq!(f.controller, Some(RelativeControl::You));
                assert_eq!(f.extra, vec![ObjectPredicate::PlayerOnly]);
            }
            other => panic!("expected Multiplier, got {other:?}"),
        }

        let re_json = serde_json::to_string(&def).unwrap();
        assert!(re_json.contains(r#""playerOnly""#));
        assert_eq!(serde_json::from_str::<StaticDef>(&re_json).unwrap(), def);

        let pc = postcard::to_allocvec(&def).unwrap();
        assert_eq!(postcard::from_bytes::<StaticDef>(&pc).unwrap(), def);
    }

    fn assert_round_trip_and_unknown_rejected<T>(valid_json: &str, bogus_json: &str)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let parsed: T = serde_json::from_str(valid_json)
            .unwrap_or_else(|e| panic!("valid JSON failed to parse ({valid_json}): {e}"));
        let re_json = serde_json::to_string(&parsed).unwrap();
        let reparsed: T = serde_json::from_str(&re_json).unwrap();
        assert_eq!(reparsed, parsed);

        let pc = postcard::to_allocvec(&parsed).unwrap();
        let from_pc: T = postcard::from_bytes(&pc).unwrap();
        assert_eq!(from_pc, parsed);

        let err = serde_json::from_str::<T>(bogus_json).expect_err(&format!(
            "expected unknown field rejection for {bogus_json}"
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field"),
            "expected 'unknown field' error for {bogus_json}, got: {msg}"
        );
    }

    #[test]
    fn all_30_struct_variant_enums_round_trip_and_deny_unknown_fields() {
        // 1. cost::CostObjectTarget
        assert_round_trip_and_unknown_rejected::<CostObjectTarget>(
            r#"{"filter":{"filter":{"types_any":2}}}"#,
            r#"{"filter":{"filter":{"types_any":2},"unknownField":1}}"#,
        );
        // 2. cost::CostDef
        assert_round_trip_and_unknown_rejected::<CostDef>(
            r#"{"payLife":{"amount":{"const":{"n":2}}}}"#,
            r#"{"payLife":{"amount":{"const":{"n":2}},"unknownField":true}}"#,
        );
        // 3. effect::Selector
        assert_round_trip_and_unknown_rejected::<Selector>(
            r#"{"target":{"index":0}}"#,
            r#"{"target":{"index":0,"unknownField":1}}"#,
        );
        // 4. effect::StepGate
        assert_round_trip_and_unknown_rejected::<StepGate>(
            r#"{"ifPrecedingAccepted":{"step_index":0}}"#,
            r#"{"ifPrecedingAccepted":{"step_index":0,"unknownField":1}}"#,
        );
        // 5. effect::ManaProduction
        assert_round_trip_and_unknown_rejected::<ManaProduction>(
            r#"{"fixed":{"symbols":["G"]}}"#,
            r#"{"fixed":{"symbols":["G"],"unknownField":1}}"#,
        );
        // 6. effect::PartialRevealSpec
        assert_round_trip_and_unknown_rejected::<PartialRevealSpec>(
            r#"{"count":{"amount":{"const":{"n":2}}}}"#,
            r#"{"count":{"amount":{"const":{"n":2}},"unknownField":1}}"#,
        );
        // 7. effect::EffectOp
        assert_round_trip_and_unknown_rejected::<EffectOp>(
            r#"{"destroy":{"cant_be_regenerated":true}}"#,
            r#"{"destroy":{"cant_be_regenerated":true,"unknownField":1}}"#,
        );
        // 8. filter::ObjectPredicate
        assert_round_trip_and_unknown_rejected::<ObjectPredicate>(
            r#"{"named":{"name":"Synth Bear"}}"#,
            r#"{"named":{"name":"Synth Bear","unknownField":1}}"#,
        );
        // 9. replace::EtbChoiceSpec
        assert_round_trip_and_unknown_rejected::<EtbChoiceSpec>(
            r#"{"subType":{"scope":"creature"}}"#,
            r#"{"subType":{"scope":"creature","unknownField":1}}"#,
        );
        // 10. replace::OpponentDiscardToBattlefield
        assert_round_trip_and_unknown_rejected::<OpponentDiscardToBattlefield>(
            r#"{"withCounters":{"counter":0,"count":1}}"#,
            r#"{"withCounters":{"counter":0,"count":1,"unknownField":1}}"#,
        );
        // 11. replace::ReplacementWith
        assert_round_trip_and_unknown_rejected::<ReplacementWith>(
            r#"{"proliferateMultiplier":{"factor":2}}"#,
            r#"{"proliferateMultiplier":{"factor":2,"unknownField":1}}"#,
        );
        // 12. static_def::Affected
        assert_round_trip_and_unknown_rejected::<Affected>(
            r#"{"filter":{"filter":{"types_any":2}}}"#,
            r#"{"filter":{"filter":{"types_any":2},"unknownField":1}}"#,
        );
        // 13. static_def::AbilityGrantSource
        assert_round_trip_and_unknown_rejected::<AbilityGrantSource>(
            r#"{"battlefield":{"filter":{"types_any":2}}}"#,
            r#"{"battlefield":{"filter":{"types_any":2},"unknownField":1}}"#,
        );
        // 14. static_def::LayerMod
        assert_round_trip_and_unknown_rejected::<LayerMod>(
            r#"{"setName":{"name":"Synth Token"}}"#,
            r#"{"setName":{"name":"Synth Token","unknownField":1}}"#,
        );
        // 15. static_def::SpellOrAbilityFilter
        assert_round_trip_and_unknown_rejected::<SpellOrAbilityFilter>(
            r#"{"spell":{"filter":{"types_any":32}}}"#,
            r#"{"spell":{"filter":{"types_any":32},"unknownField":1}}"#,
        );
        // 16. static_def::RuleMod
        assert_round_trip_and_unknown_rejected::<RuleMod>(
            r#"{"maxHandSizeDelta":{"delta":2}}"#,
            r#"{"maxHandSizeDelta":{"delta":2,"unknownField":1}}"#,
        );
        // 17. static_def::CombatRuleSpec
        assert_round_trip_and_unknown_rejected::<CombatRuleSpec>(
            r#"{"maxBlockers":{"max":2}}"#,
            r#"{"maxBlockers":{"max":2,"unknownField":1}}"#,
        );
        // 18. static_def::StaticDamageAmount
        assert_round_trip_and_unknown_rejected::<StaticDamageAmount>(
            r#"{"fixed":{"n":3}}"#,
            r#"{"fixed":{"n":3,"unknownField":1}}"#,
        );
        // 19. static_def::StaticDamageTarget
        assert_round_trip_and_unknown_rejected::<StaticDamageTarget>(
            r#"{"filter":{"filter":{"types_any":2}}}"#,
            r#"{"filter":{"filter":{"types_any":2},"unknownField":1}}"#,
        );
        // 20. static_def::StaticDamageSourceFilter
        assert_round_trip_and_unknown_rejected::<StaticDamageSourceFilter>(
            r#"{"filter":{"filter":{"types_any":2}}}"#,
            r#"{"filter":{"filter":{"types_any":2},"unknownField":1}}"#,
        );
        // 21. static_def::StaticOnPrevented
        assert_round_trip_and_unknown_rejected::<StaticOnPrevented>(
            r#"{"reflexiveDamageTrigger":{"target":{"kind":"object","min":1,"max":1}}}"#,
            r#"{"reflexiveDamageTrigger":{"target":{"kind":"object","min":1,"max":1},"unknownField":1}}"#,
        );
        // 22. static_def::StaticBody
        assert_round_trip_and_unknown_rejected::<StaticBody>(
            r#"{"multiplier":{"what":"counters","factor":2.0}}"#,
            r#"{"multiplier":{"what":"counters","factor":2.0,"unknownField":1}}"#,
        );
        // 23. trigger::TriggerSubject
        assert_round_trip_and_unknown_rejected::<TriggerSubject>(
            r#"{"filter":{"filter":{"types_any":2}}}"#,
            r#"{"filter":{"filter":{"types_any":2},"unknownField":1}}"#,
        );
        // 24. trigger::EventDetail
        assert_round_trip_and_unknown_rejected::<EventDetail>(
            r#"{"landPlayed":{"cast_from_not_hand":true}}"#,
            r#"{"landPlayed":{"cast_from_not_hand":true,"unknownField":1}}"#,
        );
        // 25. types::PrintedPt
        assert_round_trip_and_unknown_rejected::<PrintedPt>(
            r#"{"fixed":{"power":2,"toughness":3}}"#,
            r#"{"fixed":{"power":2,"toughness":3,"unknownField":1}}"#,
        );
        // 26. types::ManaPip
        assert_round_trip_and_unknown_rejected::<ManaPip>(
            r#"{"colored":{"color":"W"}}"#,
            r#"{"colored":{"color":"W","unknownField":1}}"#,
        );
        // 27. value::Ref
        assert_round_trip_and_unknown_rejected::<Ref>(
            r#"{"target":{"index":0}}"#,
            r#"{"target":{"index":0,"unknownField":1}}"#,
        );
        // 28. value::PlayerRef
        assert_round_trip_and_unknown_rejected::<PlayerRef>(
            r#"{"target":{"index":1}}"#,
            r#"{"target":{"index":1,"unknownField":1}}"#,
        );
        // 29. value::Amount
        assert_round_trip_and_unknown_rejected::<Amount>(
            r#"{"const":{"n":4}}"#,
            r#"{"const":{"n":4,"unknownField":1}}"#,
        );
        // 30. value::Condition
        assert_round_trip_and_unknown_rejected::<Condition>(
            r#"{"classLevelIs":{"level":2}}"#,
            r#"{"classLevelIs":{"level":2,"unknownField":1}}"#,
        );
    }

    #[test]
    fn cards_bin_parallel_chunked_round_trip_and_str_index_lookup() {
        let mut cards = Vec::with_capacity(5000);
        for i in 0..5000usize {
            // Include duplicate name on i == 100 and i == 4500 to verify first-insertion-wins.
            let name = if i == 100 || i == 4500 {
                "Synth Duplicate".to_string()
            } else {
                format!("Synth Card {i}")
            };
            cards.push(CardDef {
                oracle_id: format!("oracle_{i:05}"),
                compiler_version: 46,
                text_hash: format!("{i:08x}"),
                name,
                layout: CardLayout::Normal,
                faces: vec![FaceDef {
                    name: format!("Synth Face {i}"),
                    characteristics: Characteristics {
                        mana_cost: ManaCost::default(),
                        types: CardTypeMask::CREATURE,
                        subtypes: smallvec::smallvec![SubtypeId((i % 3) as u16)],
                        supertypes: SupertypeMask::empty(),
                        colors: ColorMask::G,
                        color_indicator: None,
                        pt: Some(PrintedPt::Fixed {
                            power: 2,
                            toughness: 2,
                        }),
                        loyalty: None,
                        defense: None,
                    },
                    abilities: vec![],
                    has_read_ahead: false,
                    prototype_spec: None,
                    cleaved_spell_spec: None,
                    gift_promised_spell_spec: None,
                }],
                fully_supported: i % 2 == 0,
            });
        }
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables {
                subtypes: vec!["Elf".into(), "Bear".into(), "Golem".into()],
                counters: vec!["+1/+1".into(), "lore".into()],
                keywords: vec!["flying".into(), "trample".into()],
            },
            cards,
            decks: vec![DeckDef {
                id: "d0".into(),
                name: "Synth 5k".into(),
                commanders: vec![],
                cards: vec![DeckEntry {
                    oracle_id: "oracle_00000".into(),
                    count: 4,
                }],
            }],
        };

        let db = CardDatabase::from_bundle(bundle).unwrap();
        let raw = db.encode_raw().unwrap();
        assert_eq!(&raw[0..9], &[b'M', b'T', b'G', b'C', 2, 0, 0, 0, 0]);
        let dec_raw = CardDatabase::decode_bytes(&raw).unwrap();
        assert_eq!(dec_raw.cards, db.cards);
        assert_eq!(dec_raw.decks, db.decks);
        assert_eq!(dec_raw.tables, db.tables);
        assert_eq!(dec_raw.id_by_oracle("oracle_04999"), Some(CardDefId(4999)));
        assert_eq!(dec_raw.id_by_oracle("missing_oracle"), None);
        assert_eq!(dec_raw.id_by_name("Synth Duplicate"), Some(CardDefId(100)));
        assert_eq!(dec_raw.id_by_name("Synth Card 4096"), Some(CardDefId(4096)));
        assert_eq!(dec_raw.id_by_name("missing_name"), None);
        assert_eq!(dec_raw.subtype_id("Golem"), Some(SubtypeId(2)));
        assert_eq!(dec_raw.subtype_id("Dragon"), None);
        assert_eq!(dec_raw.counter_id("lore"), Some(CounterId(1)));
        assert_eq!(dec_raw.keyword_id("trample"), Some(KeywordId(1)));

        #[cfg(feature = "zstd")]
        {
            let zst = db.encode_zstd(1).unwrap();
            assert_eq!(&zst[0..9], &[b'M', b'T', b'G', b'C', 2, 0, 0, 0, 1]);
            let dec_zst = CardDatabase::decode_bytes(&zst).unwrap();
            assert_eq!(dec_zst.cards, db.cards);
            assert_eq!(dec_zst.id_by_oracle("oracle_04096"), Some(CardDefId(4096)));
            assert_eq!(dec_zst.id_by_name("Synth Duplicate"), Some(CardDefId(100)));
        }
    }

    #[test]
    fn cards_bin_corrupted_chunk_framing_rejected() {
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards: vec![CardDef {
                oracle_id: "o1".into(),
                compiler_version: 46,
                text_hash: "h1".into(),
                name: "Synth One".into(),
                layout: CardLayout::Normal,
                faces: vec![],
                fully_supported: true,
            }],
            decks: vec![],
        };
        let db = CardDatabase::from_bundle(bundle).unwrap();
        let raw = db.encode_raw().unwrap();

        // Truncated header / body at every prefix below full length must fail.
        for len in 9..raw.len() {
            assert!(
                CardDatabase::decode_bytes(&raw[..len]).is_err(),
                "expected error for truncated cards.bin at len {len}"
            );
        }

        // Trailing garbage byte must fail.
        let mut trailing = raw.clone();
        trailing.push(0xAA);
        assert!(CardDatabase::decode_bytes(&trailing).is_err());

        // Absurd n_chunks claiming overflow must fail without OOM.
        let mut bad_chunks = Vec::from(&raw[..9]);
        let hdr_len = u32::from_le_bytes([raw[9], raw[10], raw[11], raw[12]]) as usize;
        bad_chunks.extend_from_slice(&raw[9..13 + hdr_len]);
        bad_chunks.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(CardDatabase::decode_bytes(&bad_chunks).is_err());
    }

    fn make_synth_card(i: usize) -> CardDef {
        CardDef {
            oracle_id: format!("oracle_{i:05}"),
            compiler_version: 46,
            text_hash: format!("{i:08x}"),
            name: format!("Synth Card {i}"),
            layout: CardLayout::Normal,
            faces: vec![FaceDef {
                name: format!("Synth Face {i}"),
                characteristics: Characteristics {
                    mana_cost: ManaCost::default(),
                    types: CardTypeMask::CREATURE,
                    subtypes: smallvec::smallvec![],
                    supertypes: SupertypeMask::empty(),
                    colors: ColorMask::G,
                    color_indicator: None,
                    pt: Some(PrintedPt::Fixed {
                        power: 2,
                        toughness: 2,
                    }),
                    loyalty: None,
                    defense: None,
                },
                abilities: vec![],
                has_read_ahead: false,
                prototype_spec: None,
                cleaved_spell_spec: None,
                gift_promised_spell_spec: None,
            }],
            fully_supported: i.is_multiple_of(2),
        }
    }

    fn make_synth_db(count: usize) -> CardDatabase {
        let cards = (0..count).map(make_synth_card).collect();
        let bundle = IrV2Bundle {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: InternTables::default(),
            cards,
            decks: vec![],
        };
        CardDatabase::from_bundle(bundle).unwrap()
    }

    fn build_raw_cards_bin_from_chunks(
        num_cards: u32,
        compression: u8,
        hdr_extra: &[u8],
        chunks: &[Vec<u8>],
    ) -> Vec<u8> {
        let tables = InternTables::default();
        let decks: Vec<DeckDef> = Vec::new();
        let hdr = ChunkedHeader {
            ir_version: IR_V2_VERSION,
            source_compiler_version: 46,
            tables: &tables,
            decks: &decks,
            num_cards,
        };
        let mut hdr_bytes = postcard::to_allocvec(&hdr).unwrap();
        hdr_bytes.extend_from_slice(hdr_extra);
        let hdr_len = u32::try_from(hdr_bytes.len()).unwrap();
        let n_chunks = u32::try_from(chunks.len()).unwrap();

        let mut out = Vec::new();
        out.extend_from_slice(&CARDS_BIN_MAGIC);
        out.extend_from_slice(&IR_V2_VERSION.to_le_bytes());
        out.push(compression);
        out.extend_from_slice(&hdr_len.to_le_bytes());
        out.extend_from_slice(&hdr_bytes);
        out.extend_from_slice(&n_chunks.to_le_bytes());
        for c in chunks {
            out.extend_from_slice(&(c.len() as u32).to_le_bytes());
        }
        for c in chunks {
            out.extend_from_slice(c);
        }
        out
    }

    fn wire_chunk_count(bytes: &[u8]) -> u32 {
        let hdr_len = u32::from_le_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]) as usize;
        let pos = 13 + hdr_len;
        u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
    }

    fn assert_invalid_data(res: Result<CardDatabase, IrCodecError>, ctx: &str) {
        match res {
            Err(IrCodecError::Io(ref e)) if e.kind() == std::io::ErrorKind::InvalidData => {}
            other => panic!("{ctx}: expected InvalidData, got {other:?}"),
        }
    }

    #[test]
    fn cards_bin_rejects_oom_num_cards_with_zero_chunks() {
        for num_cards in [u32::MAX, u32::MAX - 1, 100_000_000, 1] {
            let payload = build_raw_cards_bin_from_chunks(num_cards, COMPRESSION_NONE, &[], &[]);
            assert_invalid_data(
                CardDatabase::decode_bytes(&payload),
                &format!("num_cards={num_cards}, n_chunks=0"),
            );
        }
    }

    #[test]
    fn cards_bin_rejects_hdr_num_cards_mismatch_against_chunks() {
        for actual_n in [1usize, 4096usize] {
            let db = make_synth_db(actual_n);
            let raw_chunks: Vec<Vec<u8>> = if actual_n == 1 {
                vec![postcard::to_allocvec(&db.cards).unwrap()]
            } else {
                let cs = db.cards.len().div_ceil(PARALLEL_CHUNKS);
                db.cards
                    .chunks(cs)
                    .map(|sl| postcard::to_allocvec(sl).unwrap())
                    .collect()
            };

            let bad_counts = [
                0u32,
                (actual_n - 1) as u32,
                (actual_n + 1) as u32,
                100_000_000,
                u32::MAX,
            ];
            for bad_n in bad_counts {
                let raw_buf =
                    build_raw_cards_bin_from_chunks(bad_n, COMPRESSION_NONE, &[], &raw_chunks);
                assert_invalid_data(
                    CardDatabase::decode_bytes(&raw_buf),
                    &format!("raw actual_n={actual_n}, bad_n={bad_n}"),
                );
            }

            #[cfg(feature = "zstd")]
            {
                let zst_chunks: Vec<Vec<u8>> = raw_chunks
                    .iter()
                    .map(|c| zstd::bulk::compress(c, 1).unwrap())
                    .collect();
                for bad_n in bad_counts {
                    let zst_buf =
                        build_raw_cards_bin_from_chunks(bad_n, COMPRESSION_ZSTD, &[], &zst_chunks);
                    assert_invalid_data(
                        CardDatabase::decode_bytes(&zst_buf),
                        &format!("zstd actual_n={actual_n}, bad_n={bad_n}"),
                    );
                }
            }
        }
    }

    #[test]
    fn cards_bin_rejects_trailing_bytes_and_empty_or_excess_chunks() {
        let one_card = vec![make_synth_card(0)];
        let mut valid_chunk = postcard::to_allocvec(&one_card).unwrap();

        // Trailing byte inside hdr_len.
        let bad_hdr = build_raw_cards_bin_from_chunks(
            1,
            COMPRESSION_NONE,
            &[0xFF],
            std::slice::from_ref(&valid_chunk),
        );
        assert_invalid_data(CardDatabase::decode_bytes(&bad_hdr), "trailing hdr byte");

        // Trailing byte inside raw chunk payload.
        let mut trailing_chunk = valid_chunk.clone();
        trailing_chunk.push(0x00);
        let bad_chunk =
            build_raw_cards_bin_from_chunks(1, COMPRESSION_NONE, &[], &[trailing_chunk]);
        assert_invalid_data(CardDatabase::decode_bytes(&bad_chunk), "trailing raw chunk");

        // Empty card vec chunk `[0x00]` with num_cards=0 or num_cards=1.
        let empty_vec_chunk = postcard::to_allocvec(&Vec::<CardDef>::new()).unwrap();
        assert_eq!(empty_vec_chunk, vec![0x00]);
        for nc in [0u32, 1u32] {
            let p = build_raw_cards_bin_from_chunks(
                nc,
                COMPRESSION_NONE,
                &[],
                std::slice::from_ref(&empty_vec_chunk),
            );
            assert_invalid_data(
                CardDatabase::decode_bytes(&p),
                &format!("empty chunk num_cards={nc}"),
            );
        }

        // n_chunks > num_cards (2 chunks for 1 card).
        let excess = build_raw_cards_bin_from_chunks(
            1,
            COMPRESSION_NONE,
            &[],
            &[valid_chunk.clone(), std::mem::take(&mut valid_chunk)],
        );
        assert_invalid_data(CardDatabase::decode_bytes(&excess), "n_chunks > num_cards");

        #[cfg(feature = "zstd")]
        {
            let mut z_trail = postcard::to_allocvec(&one_card).unwrap();
            z_trail.push(0x42);
            let z_chunk = zstd::bulk::compress(&z_trail, 1).unwrap();
            let bad_z = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[z_chunk]);
            assert_invalid_data(CardDatabase::decode_bytes(&bad_z), "trailing zstd chunk");
        }
    }

    #[test]
    fn cards_bin_bounded_threads_for_many_small_chunks() {
        let count = 4096usize;
        let single_chunks: Vec<Vec<u8>> = (0..count)
            .map(|i| postcard::to_allocvec(&vec![make_synth_card(i)]).unwrap())
            .collect();
        let raw =
            build_raw_cards_bin_from_chunks(count as u32, COMPRESSION_NONE, &[], &single_chunks);
        let decoded = CardDatabase::decode_bytes(&raw).unwrap();
        assert_eq!(decoded.cards.len(), count);
        assert_eq!(decoded.id_by_oracle("oracle_04095"), Some(CardDefId(4095)));

        #[cfg(feature = "zstd")]
        {
            let z_chunks: Vec<Vec<u8>> = single_chunks
                .iter()
                .map(|c| zstd::bulk::compress(c, 1).unwrap())
                .collect();
            let zst =
                build_raw_cards_bin_from_chunks(count as u32, COMPRESSION_ZSTD, &[], &z_chunks);
            let dec_zst = CardDatabase::decode_bytes(&zst).unwrap();
            assert_eq!(dec_zst.cards.len(), count);
            assert_eq!(dec_zst.id_by_name("Synth Card 4095"), Some(CardDefId(4095)));
        }
    }

    #[test]
    fn cards_bin_raw_decode_limits_and_no_zstd_feature_guard() {
        let card = make_synth_card(0);
        let chunk = postcard::to_allocvec(&vec![card]).unwrap();
        assert!(chunk.len() > 64);

        let one =
            build_raw_cards_bin_from_chunks(1, COMPRESSION_NONE, &[], std::slice::from_ref(&chunk));
        let tight_chunk_limit = DecodeLimits {
            max_chunk_raw_bytes: 64,
            max_total_raw_bytes: 128,
        };
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&one, tight_chunk_limit),
            "raw chunk exceeds max_chunk_raw_bytes",
        );

        let three_chunks = vec![chunk.clone(), chunk.clone(), chunk];
        let three = build_raw_cards_bin_from_chunks(3, COMPRESSION_NONE, &[], &three_chunks);
        let tight_total_limit = DecodeLimits {
            max_chunk_raw_bytes: 1024,
            max_total_raw_bytes: 128,
        };
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&three, tight_total_limit),
            "raw chunks exceed max_total_raw_bytes",
        );

        #[cfg(not(feature = "zstd"))]
        {
            let zst_stub = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[vec![1]]);
            assert!(matches!(
                CardDatabase::decode_bytes(&zst_stub),
                Err(IrCodecError::ZstdUnavailable)
            ));
        }
    }

    #[cfg(feature = "zstd")]
    fn zstd_stream_compress(data: &[u8], level: i32) -> Vec<u8> {
        use std::io::Write as _;
        let mut enc = zstd::stream::Encoder::new(Vec::new(), level).unwrap();
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[cfg(feature = "zstd")]
    fn forged_zstd_header(declared_size: u64) -> Vec<u8> {
        // Zstd frame: magic (4B) + FHD 0xE0 (8-byte FCS + Single_Segment) + FCS (8B LE) + empty last raw block (3B)
        let mut frame = Vec::with_capacity(16);
        frame.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
        frame.push(0xE0);
        frame.extend_from_slice(&declared_size.to_le_bytes());
        frame.extend_from_slice(&[0x01, 0x00, 0x00]);
        frame
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn cards_bin_rejects_forged_and_empty_zstd_headers() {
        // 1. Forged 10 GiB header
        let ten_gib = 10u64 * 1024 * 1024 * 1024;
        let forged_10g = forged_zstd_header(ten_gib);
        assert!(matches!(
            zstd::zstd_safe::get_frame_content_size(&forged_10g),
            Ok(Some(v)) if v == ten_gib
        ));
        let p_10g = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[forged_10g]);
        assert_invalid_data(CardDatabase::decode_bytes(&p_10g), "forged 10 GiB zstd");

        // 2. Forged MAX_CHUNK_RAW_BYTES + 1 header
        let over_max = (MAX_CHUNK_RAW_BYTES as u64) + 1;
        let forged_over = forged_zstd_header(over_max);
        let p_over = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[forged_over]);
        assert_invalid_data(
            CardDatabase::decode_bytes(&p_over),
            "forged MAX_CHUNK_RAW_BYTES+1 zstd",
        );

        // 3. Forged MAX_CHUNK_RAW_BYTES header on corrupt 32-byte payload (must fail without 128 MiB alloc)
        let mut forged_max_corrupt = Vec::new();
        forged_max_corrupt.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD, 0xE0]);
        forged_max_corrupt.extend_from_slice(&(MAX_CHUNK_RAW_BYTES as u64).to_le_bytes());
        forged_max_corrupt.extend_from_slice(&[0xFF; 19]);
        let p_corrupt =
            build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[forged_max_corrupt]);
        assert!(CardDatabase::decode_bytes(&p_corrupt).is_err());

        // 4. Ok(Some(0)), truncated header, empty chunk
        let zero_frame = zstd::bulk::compress(&[], 3).unwrap();
        assert!(matches!(
            zstd::zstd_safe::get_frame_content_size(&zero_frame),
            Ok(Some(0))
        ));
        for (label, chunk) in [
            ("Ok(Some(0))", zero_frame.clone()),
            ("truncated zstd header", vec![0x28, 0xB5, 0x2F]),
            ("empty zstd chunk", Vec::new()),
        ] {
            let p = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[chunk]);
            assert_invalid_data(CardDatabase::decode_bytes(&p), label);
        }

        // 5. [0-byte frame] ++ [streaming bomb]
        let small_stream = zstd_stream_compress(&vec![0u8; 4096], 1);
        let zero_plus_bomb = [zero_frame, small_stream.clone()].concat();
        let p_zero_bomb =
            build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[zero_plus_bomb]);
        assert_invalid_data(
            CardDatabase::decode_bytes(&p_zero_bomb),
            "[0-byte]++[streaming bomb]",
        );

        // 6. Concatenated [1-byte frame] ++ [streaming 4096-byte frame]
        let one_byte_frame = zstd::bulk::compress(&[0x00], 3).unwrap();
        let multi_frame = [one_byte_frame, small_stream].concat();
        let p_multi = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[multi_frame]);
        assert!(CardDatabase::decode_bytes(&p_multi).is_err());
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn cards_bin_streaming_zstd_frames_and_bomb_guard() {
        // 1. Valid streaming frame Ok(None) succeeds.
        let mut card = make_synth_card(0);
        let raw_base = postcard::to_allocvec(&vec![card.clone()]).unwrap();
        let pad_needed = 1024usize.saturating_sub(raw_base.len());
        card.text_hash.push_str(&"a".repeat(pad_needed));
        let exact_1024 = postcard::to_allocvec(&vec![card.clone()]).unwrap();
        // Adjust if varint length byte grew by 1 when crossing 128 bytes.
        let exact_1024 = if exact_1024.len() > 1024 {
            let trim = exact_1024.len() - 1024;
            card.text_hash.truncate(card.text_hash.len() - trim);
            postcard::to_allocvec(&vec![card.clone()]).unwrap()
        } else {
            exact_1024
        };
        assert_eq!(exact_1024.len(), 1024);

        let stream_ok = zstd_stream_compress(&exact_1024, 3);
        assert!(matches!(
            zstd::zstd_safe::get_frame_content_size(&stream_ok),
            Ok(None)
        ));

        let p_ok = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[stream_ok]);
        let limits = DecodeLimits {
            max_chunk_raw_bytes: 1024,
            max_total_raw_bytes: 4096,
        };
        let dec_ok = CardDatabase::decode_bytes_with_limits(&p_ok, limits).unwrap();
        assert_eq!(dec_ok.cards.len(), 1);
        assert_eq!(dec_ok.id_by_oracle("oracle_00000"), Some(CardDefId(0)));

        // 2. Exact boundary 1025 bytes > 1024 fails with InvalidData.
        let stream_over = zstd_stream_compress(&vec![0u8; 1025], 1);
        assert!(matches!(
            zstd::zstd_safe::get_frame_content_size(&stream_over),
            Ok(None)
        ));
        let p_over = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[stream_over]);
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&p_over, limits),
            "streaming 1025 > 1024",
        );

        // 3. Real MAX_CHUNK_RAW_BYTES + 1024 streaming zstd bomb on public CardDatabase::decode_bytes.
        let bomb_raw = vec![0u8; MAX_CHUNK_RAW_BYTES + 1024];
        let bomb_zst = zstd_stream_compress(&bomb_raw, 1);
        assert!(matches!(
            zstd::zstd_safe::get_frame_content_size(&bomb_zst),
            Ok(None)
        ));
        let p_bomb = build_raw_cards_bin_from_chunks(1, COMPRESSION_ZSTD, &[], &[bomb_zst]);
        assert_invalid_data(
            CardDatabase::decode_bytes(&p_bomb),
            "streaming 128 MiB + 1024 zstd bomb",
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn cards_bin_zstd_global_total_raw_bytes_cap() {
        let limits = DecodeLimits {
            max_chunk_raw_bytes: 1024,
            max_total_raw_bytes: 2048,
        };

        let mut card = make_synth_card(0);
        card.text_hash = "x".repeat(500);
        let raw_600 = postcard::to_allocvec(&vec![card]).unwrap();
        assert!(raw_600.len() > 512 && raw_600.len() <= 1024);

        let bulk_600 = zstd::bulk::compress(&raw_600, 1).unwrap();
        let stream_600 = zstd_stream_compress(&raw_600, 1);

        // Sequential path (num_cards = 4, n_chunks = 4): 4 * ~600 > 2048 -> InvalidData
        let seq_bulk =
            build_raw_cards_bin_from_chunks(4, COMPRESSION_ZSTD, &[], &vec![bulk_600.clone(); 4]);
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&seq_bulk, limits),
            "sequential bulk exceeds max_total_raw_bytes",
        );

        let seq_stream =
            build_raw_cards_bin_from_chunks(4, COMPRESSION_ZSTD, &[], &vec![stream_600.clone(); 4]);
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&seq_stream, limits),
            "sequential streaming exceeds max_total_raw_bytes",
        );

        // Parallel path (num_cards = 4096, n_chunks = 16): 16 * ~600 > 2048 -> InvalidData
        let par_bulk =
            build_raw_cards_bin_from_chunks(4096, COMPRESSION_ZSTD, &[], &vec![bulk_600; 16]);
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&par_bulk, limits),
            "parallel bulk exceeds max_total_raw_bytes",
        );

        let par_stream =
            build_raw_cards_bin_from_chunks(4096, COMPRESSION_ZSTD, &[], &vec![stream_600; 16]);
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&par_stream, limits),
            "parallel streaming exceeds max_total_raw_bytes",
        );

        // Two valid 1024-byte chunks exhaust the 2048-byte global cap, then a forged Ok(Some(1024))
        // must fail at reserve_raw_bytes (InvalidData) BEFORE zstd tries to decompress the corrupt body.
        let mut card_1024 = make_synth_card(1);
        let base_len = postcard::to_allocvec(&vec![card_1024.clone()])
            .unwrap()
            .len();
        card_1024
            .text_hash
            .push_str(&"b".repeat(1024usize.saturating_sub(base_len)));
        let mut raw_1024 = postcard::to_allocvec(&vec![card_1024.clone()]).unwrap();
        if raw_1024.len() > 1024 {
            let trim = raw_1024.len() - 1024;
            card_1024
                .text_hash
                .truncate(card_1024.text_hash.len() - trim);
            raw_1024 = postcard::to_allocvec(&vec![card_1024]).unwrap();
        }
        assert_eq!(raw_1024.len(), 1024);
        let bulk_1024 = zstd::bulk::compress(&raw_1024, 1).unwrap();
        let forged_1024 = forged_zstd_header(1024);
        let seq_forged = build_raw_cards_bin_from_chunks(
            3,
            COMPRESSION_ZSTD,
            &[],
            &[bulk_1024.clone(), bulk_1024, forged_1024],
        );
        assert_invalid_data(
            CardDatabase::decode_bytes_with_limits(&seq_forged, limits),
            "forged Ok(Some(1024)) after 2048 bytes fails at reserve_raw_bytes",
        );
    }

    #[test]
    fn cards_bin_chunk_threshold_boundaries() {
        let cases = [
            (0usize, 0u32),
            (1, 1),
            (4095, 1),
            (4096, PARALLEL_CHUNKS as u32),
            (4097, PARALLEL_CHUNKS as u32),
        ];
        for (count, expected_chunks) in cases {
            let db = make_synth_db(count);
            let raw = db.encode_raw().unwrap();
            assert_eq!(
                wire_chunk_count(&raw),
                expected_chunks,
                "raw chunk count for {count} cards"
            );
            let dec_raw = CardDatabase::decode_bytes(&raw).unwrap();
            assert_eq!(dec_raw.cards, db.cards);
            if count > 0 {
                let last = count - 1;
                let oid = format!("oracle_{last:05}");
                let name = format!("Synth Card {last}");
                assert_eq!(dec_raw.id_by_oracle(&oid), Some(CardDefId(last as u32)));
                assert_eq!(dec_raw.id_by_name(&name), Some(CardDefId(last as u32)));
            }

            #[cfg(feature = "zstd")]
            {
                let zst = db.encode_zstd(1).unwrap();
                assert_eq!(
                    wire_chunk_count(&zst),
                    expected_chunks,
                    "zstd chunk count for {count} cards"
                );
                let dec_zst = CardDatabase::decode_bytes(&zst).unwrap();
                assert_eq!(dec_zst.cards, db.cards);
                if count > 0 {
                    let last = count - 1;
                    let oid = format!("oracle_{last:05}");
                    let name = format!("Synth Card {last}");
                    assert_eq!(dec_zst.id_by_oracle(&oid), Some(CardDefId(last as u32)));
                    assert_eq!(dec_zst.id_by_name(&name), Some(CardDefId(last as u32)));
                }
            }
        }
    }
}
