use std::sync::OnceLock;

use mtg_ir::{CardDef, CounterId, InternTables, KeywordId, SubtypeId};
use serde::{Deserialize, Serialize};

use crate::error::CompileError;
use crate::intern::{IdRemapper, Interner, remap_card_ids};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverrideCardEntry {
    pub tables: InternTables,
    pub card: CardDef,
}

pub struct EmbeddedOverride {
    pub oracle_id: &'static str,
    pub expected_text_hash: &'static str,
    json: &'static str,
    parsed: OnceLock<OverrideCardEntry>,
}

macro_rules! override_slot {
    ($oracle_id:literal, $expected_hash:literal) => {
        EmbeddedOverride {
            oracle_id: $oracle_id,
            expected_text_hash: $expected_hash,
            json: include_str!(concat!("../data/overrides/", $oracle_id, ".json")),
            parsed: OnceLock::new(),
        }
    };
}

/// All 38 hash-gated IR v2 overrides, sorted lexicographically by `oracle_id`.
pub static OVERRIDES: [EmbeddedOverride; 38] = [
    override_slot!("0636b6c3-0662-420a-b30d-f0a14e7c512d", "db593f4b"), // Mirkwood Bats
    override_slot!("08becc07-28bc-4a2f-a6b0-28a2998d2f50", "b1259b19"), // Windfall
    override_slot!("1313da2f-92bb-4862-9153-373972cc1520", "383f70b9"), // Descent into Avernus
    override_slot!("15d5b198-edbe-40f3-87e2-d72f7f0c58ab", "738e4f34"), // Dance of the Dead
    override_slot!("240e85d3-e495-4877-8609-4b4056c402f7", "77cd9f22"), // Victimize
    override_slot!("2962fe4c-bf48-454b-8a6b-0f8253352ae8", "bff68e7f"), // Spelunking
    override_slot!("32fbb638-ab14-4e8b-a07a-d4c44e3496f2", "620bae3f"), // Urza's Tower
    override_slot!("333745d9-e930-439b-94d6-3aeea2877f69", "ae7920f5"), // Illusionist's Gambit
    override_slot!("33e85a8a-86df-4cdc-a9cc-8cbabe92c3c0", "0a0e3d32"), // Urza's Mine
    override_slot!("3d92e5ce-9d30-4203-9f84-6c36117440cf", "dabdfb28"), // Spellweaver Volute
    override_slot!("48534bbf-06b6-46a0-8a86-b14f38caee58", "1f52c171"), // Sin, Unending Cataclysm
    override_slot!("49d4aef7-d2d1-4f66-bc2a-79ec8cc96565", "80127b9f"), // Panglacial Wurm
    override_slot!("4ff07b7c-e97a-4b57-bcf1-2f22c37a8bd6", "97adf131"), // Flare of Fortitude
    override_slot!("5f15125e-ae89-471e-b055-226a783bc7c1", "085536f8"), // Aetherflux Reservoir
    override_slot!("699b192c-f3f2-4ac5-9ca9-e35bcf08ad8a", "7214e43b"), // Necromancy
    override_slot!("6f4ac4a4-53ec-4bc9-8f5c-d4b801d867b2", "7b12f81a"), // Grave Pact
    override_slot!("723a66c3-2c2c-411c-bba5-922e8601981b", "8244ea97"), // Indominus Rex, Alpha
    override_slot!("85ccae95-92c4-41ff-96fd-dcb515dc568e", "f4542288"), // Path of the Animist
    override_slot!("8f351961-69e3-4637-b010-d13f19daca39", "a855a569"), // Bigger on the Inside
    override_slot!("9b9a10ff-5a5d-4df8-88aa-18d84ff9117c", "1a87f6ef"), // Inspiring Call
    override_slot!("9f6828e3-39d9-45d9-9bf1-0e3737a0321e", "2f3d3d65"), // Ulalek, Fused Atrocity
    override_slot!("a54f0869-94c8-42af-9080-166efb9486a4", "635e437a"), // Gamble
    override_slot!("aa959340-c869-4caa-92c7-572bd8d23eef", "821261c8"), // Field of the Dead
    override_slot!("ab8d5f5c-1976-4f77-8ed2-8d28ee666741", "cda06806"), // Tireless Provisioner
    override_slot!("c0d8fef4-65f4-4769-982d-b397d2b7e977", "e44a14a0"), // Animate Dead
    override_slot!("c12e2563-4ad4-4637-b9ae-72f23cca31d8", "e543202d"), // Benthic Anomaly
    override_slot!("c549b0fd-1e08-4873-952e-a14dc45a0fd2", "088e259a"), // Champion of Lambholt
    override_slot!("c823e687-6311-4c99-974b-fd77d204141a", "3e508aa2"), // Timetwister
    override_slot!("cfca44cb-8ca5-4ebb-bd5c-b621eca41944", "ea123a66"), // Space Beleren
    override_slot!("e0cef79c-ad47-4cbc-9d73-a913e487ccb7", "31a93c7b"), // Garruk, Cursed Huntsman
    override_slot!("e11966cd-2ee3-4df4-b099-abf42dcdf0db", "174bd0bd"), // Urza's Power Plant
    override_slot!("e8863518-0bfa-49c3-8c6e-6c9116a81051", "e22d5724"), // Worldly Tutor
    override_slot!("ee6099b0-fb1f-42f1-b862-7708c6e36d05", "1b302029"), // Lion's Eye Diamond
    override_slot!("f36d1d8b-8303-44a9-ab56-531931641ea2", "4ba458fb"), // Academy Manufactor
    override_slot!("fb60739e-1dc3-481d-a056-ad72e665c680", "7b023cda"), // Rite of Replication
    override_slot!("fb773114-6161-46a3-ab30-1853958fbcb0", "3130c7b2"), // Bello, Bard of the Brambles
    override_slot!("fb88fb3d-6a85-4bb1-b316-7f759b45a0fa", "63533634"), // The Chain Veil
    override_slot!("fd949f82-fc10-4e37-8aa9-6c7569fe3c55", "3d90b106"), // Akroma's Will
];

fn find_slot(oracle_id: &str) -> Option<&'static EmbeddedOverride> {
    let idx = OVERRIDES
        .binary_search_by(|slot| slot.oracle_id.cmp(oracle_id))
        .ok()?;
    Some(&OVERRIDES[idx])
}

pub fn get_or_parse(slot: &EmbeddedOverride) -> Result<&OverrideCardEntry, CompileError> {
    if let Some(entry) = slot.parsed.get() {
        return Ok(entry);
    }
    let entry: OverrideCardEntry =
        serde_json::from_str(slot.json).map_err(|e| CompileError::InvalidOverride {
            oracle_id: slot.oracle_id.to_string(),
            message: e.to_string(),
        })?;
    let _ = slot.parsed.set(entry);
    Ok(slot
        .parsed
        .get()
        .expect("OnceLock populated immediately prior"))
}

struct TableInternRemapper<'a> {
    src: &'a InternTables,
    dst: &'a mut Interner,
}

impl IdRemapper for TableInternRemapper<'_> {
    fn remap_subtype(&mut self, id: SubtypeId) -> Result<SubtypeId, CompileError> {
        let name = self
            .src
            .subtypes
            .get(id.0 as usize)
            .ok_or(CompileError::InvalidInternId {
                kind: "subtype",
                id: id.0,
                table_len: self.src.subtypes.len(),
            })?;
        Ok(self.dst.intern_subtype(name))
    }

    fn remap_counter(&mut self, id: CounterId) -> Result<CounterId, CompileError> {
        let name = self
            .src
            .counters
            .get(id.0 as usize)
            .ok_or(CompileError::InvalidInternId {
                kind: "counter",
                id: id.0,
                table_len: self.src.counters.len(),
            })?;
        Ok(self.dst.intern_counter(name))
    }

    fn remap_keyword(&mut self, id: KeywordId) -> Result<KeywordId, CompileError> {
        let name = self
            .src
            .keywords
            .get(id.0 as usize)
            .ok_or(CompileError::InvalidInternId {
                kind: "keyword",
                id: id.0,
                table_len: self.src.keywords.len(),
            })?;
        Ok(self.dst.intern_keyword(name))
    }
}

/// Resolves a hash-gated IR v2 override when `oracle_id` and `text_hash` match.
/// Returns `Ok(None)` on miss or `text_hash` mismatch so the caller falls back cleanly.
pub fn resolve_override(
    oracle_id: &str,
    text_hash: &str,
    card_name: &str,
    interner: &mut Interner,
) -> Result<Option<CardDef>, CompileError> {
    let Some(slot) = find_slot(oracle_id) else {
        return Ok(None);
    };
    if slot.expected_text_hash != text_hash {
        return Ok(None);
    }
    let entry = get_or_parse(slot)?;
    let mut card = entry.card.clone();
    card.compiler_version = crate::COMPILER_VERSION;
    card.name = card_name.to_string();
    if card.faces.len() == 1 {
        card.faces[0].name = card_name.to_string();
    }
    let mut remapper = TableInternRemapper {
        src: &entry.tables,
        dst: interner,
    };
    remap_card_ids(&mut card, &mut remapper)?;
    Ok(Some(card))
}
