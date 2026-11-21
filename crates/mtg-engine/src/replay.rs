use std::collections::BTreeMap;

use mtg_ir::{GamePhase, Zone};
use serde::{Deserialize, Serialize};

use crate::action::{Action, DeckSpec};
use crate::config::GameConfig;
use crate::id::PlayerId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub seq: u64,
    pub actor: PlayerId,
    pub action: Action,
}

/// Append-only deterministic replay stored by `magic-web` SQLite and used for bug repros / AI data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayLog {
    pub config: GameConfig,
    #[serde(default)]
    pub decks: Vec<DeckSpec>,
    pub seed: u64,
    pub entries: Vec<ReplayEntry>,
}

/// Trace recorded by `peckhamr.com/scripts/record-ts-replay.ts` for Phase 0–2 differential testing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplayTrace {
    pub format_version: u32,
    pub compiler_version: u32,
    pub seed: u32,
    pub post_init_rng_state: u32,
    pub rules: TsReplayRules,
    pub seats: Vec<TsReplaySeat>,
    pub steps: Vec<TsReplayStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplayRules {
    pub starting_life: i32,
    pub commander: bool,
    pub free_first_mulligan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplaySeat {
    pub seat: u8,
    pub deck_id: String,
    pub commanders: Vec<String>,
    pub library_order_initial: Vec<String>,
    pub hand_initial: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsReplayStep {
    pub step_index: u32,
    pub actor_seat: u8,
    pub action: serde_json::Value,
    pub object_locators: BTreeMap<String, ObjectLocator>,
    pub digest_after: TsStepDigest,
}

/// Canonical object locator mapping an opaque TS `instanceId` to a Rust game object.
///
/// Covers all 7 zones (including [`Zone::Stack`]) via `ZoneSlot` / `OwnerOrdinal` and
/// tracks card/token creation identities via `InitialCard` / `Token`, while rejecting
/// empty objects and unknown fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ObjectLocator {
    ZoneSlot(ZoneSlotLocator),
    OwnerOrdinal(OwnerOrdinalLocator),
    InitialCard(InitialCardLocator),
    Token(TokenLocator),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ZoneSlotLocator {
    pub seat: u8,
    pub zone: Zone,
    pub zone_index: usize,
    pub oracle_id: String,
    #[serde(default)]
    pub initial_card_ordinal: Option<u32>,
    #[serde(default)]
    pub token_seq: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerOrdinalLocator {
    pub owner_seat: u8,
    pub zone: Zone,
    pub ordinal: usize,
    pub oracle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitialCardLocator {
    pub initial_card_ordinal: u32,
    #[serde(default)]
    pub token_seq: Option<u32>,
    #[serde(default)]
    pub seat: Option<u8>,
    #[serde(default)]
    pub oracle_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenLocator {
    pub token_seq: u32,
    #[serde(default)]
    pub initial_card_ordinal: Option<u32>,
    #[serde(default)]
    pub seat: Option<u8>,
    #[serde(default)]
    pub oracle_id: Option<String>,
}

/// Phase reported in [`TsStepDigest`], extending [`mtg_ir::GamePhase`] with `mulligan`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TsReplayPhase {
    Mulligan,
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

impl TsReplayPhase {
    pub fn as_game_phase(self) -> Option<GamePhase> {
        match self {
            Self::Mulligan => None,
            Self::Untap => Some(GamePhase::Untap),
            Self::Upkeep => Some(GamePhase::Upkeep),
            Self::Draw => Some(GamePhase::Draw),
            Self::Main1 => Some(GamePhase::Main1),
            Self::BeginCombat => Some(GamePhase::BeginCombat),
            Self::DeclareAttackers => Some(GamePhase::DeclareAttackers),
            Self::DeclareBlockers => Some(GamePhase::DeclareBlockers),
            Self::FirstStrikeDamage => Some(GamePhase::FirstStrikeDamage),
            Self::CombatDamage => Some(GamePhase::CombatDamage),
            Self::EndCombat => Some(GamePhase::EndCombat),
            Self::Main2 => Some(GamePhase::Main2),
            Self::EndStep => Some(GamePhase::EndStep),
            Self::Cleanup => Some(GamePhase::Cleanup),
        }
    }
}

impl From<GamePhase> for TsReplayPhase {
    fn from(phase: GamePhase) -> Self {
        match phase {
            GamePhase::Untap => Self::Untap,
            GamePhase::Upkeep => Self::Upkeep,
            GamePhase::Draw => Self::Draw,
            GamePhase::Main1 => Self::Main1,
            GamePhase::BeginCombat => Self::BeginCombat,
            GamePhase::DeclareAttackers => Self::DeclareAttackers,
            GamePhase::DeclareBlockers => Self::DeclareBlockers,
            GamePhase::FirstStrikeDamage => Self::FirstStrikeDamage,
            GamePhase::CombatDamage => Self::CombatDamage,
            GamePhase::EndCombat => Self::EndCombat,
            GamePhase::Main2 => Self::Main2,
            GamePhase::EndStep => Self::EndStep,
            GamePhase::Cleanup => Self::Cleanup,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsStepDigest {
    pub turn_number: u32,
    pub active_seat: u8,
    pub priority_seat: u8,
    pub phase: TsReplayPhase,
    pub stack_depth: u16,
    pub pending_prompt_kind: Option<String>,
    pub players: Vec<TsPlayerDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsPlayerDigest {
    pub seat: u8,
    pub life: i32,
    pub lost: bool,
    pub poison: u16,
    pub energy: u16,
    pub mana_pool: [u32; 6],
    pub zone_counts: TsZoneCounts,
    pub hand_oracle_ids: Vec<String>,
    pub battlefield: Vec<TsBattlefieldEntryDigest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsZoneCounts {
    pub library: u32,
    pub hand: u32,
    pub battlefield: u32,
    pub graveyard: u32,
    pub exile: u32,
    pub command: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TsBattlefieldEntryDigest {
    pub oracle_id: String,
    pub tapped: bool,
    pub pt: Option<[i32; 2]>,
    pub types: Vec<String>,
    pub keywords: Vec<String>,
    pub counters: Vec<(String, u32)>,
    pub damage_marked: i32,
    pub attached_to_oracle_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_player_digest(seat: u8) -> serde_json::Value {
        serde_json::json!({
            "seat": seat,
            "life": 40,
            "lost": false,
            "poison": 0,
            "energy": 2,
            "manaPool": [1, 0, 2, 0, 0, 1],
            "zoneCounts": {
                "library": 91,
                "hand": 7,
                "battlefield": 2,
                "graveyard": 1,
                "exile": 0,
                "command": 1
            },
            "handOracleIds": ["synth_land_0", "synth_spell_1"],
            "battlefield": [
                {
                    "oracleId": "synth_creature_0",
                    "tapped": false,
                    "pt": [3, 4],
                    "types": ["Creature"],
                    "keywords": ["trample", "vigilance"],
                    "counters": [["+1/+1", 1]],
                    "damageMarked": 1,
                    "attachedToOracleId": null
                },
                {
                    "oracleId": "synth_aura_0",
                    "tapped": true,
                    "pt": null,
                    "types": ["Enchantment"],
                    "keywords": [],
                    "counters": [],
                    "damageMarked": 0,
                    "attachedToOracleId": "synth_creature_0"
                }
            ]
        })
    }

    #[test]
    fn ts_replay_trace_round_trip_with_all_locator_variants() {
        let trace_json = serde_json::json!({
            "formatVersion": 1,
            "compilerVersion": 46,
            "seed": 42,
            "postInitRngState": 3_141_592_653u32,
            "rules": {
                "startingLife": 40,
                "commander": true,
                "freeFirstMulligan": true
            },
            "seats": [
                {
                    "seat": 0,
                    "deckId": "synth_deck_0",
                    "commanders": ["synth_cmd_0"],
                    "libraryOrderInitial": ["synth_land_0", "synth_spell_1"],
                    "handInitial": ["synth_land_0"]
                }
            ],
            "steps": [
                {
                    "stepIndex": 0,
                    "actorSeat": 0,
                    "action": { "type": "mulligan_keep" },
                    "objectLocators": {},
                    "digestAfter": {
                        "turnNumber": 1,
                        "activeSeat": 0,
                        "prioritySeat": 0,
                        "phase": "mulligan",
                        "stackDepth": 0,
                        "pendingPromptKind": "mulligan",
                        "players": [sample_player_digest(0)]
                    }
                },
                {
                    "stepIndex": 1,
                    "actorSeat": 0,
                    "action": { "type": "cast_spell", "instanceId": "crd_1" },
                    "objectLocators": {
                        "crd_1": {
                            "seat": 0,
                            "zone": "hand",
                            "zoneIndex": 1,
                            "oracleId": "synth_spell_1"
                        },
                        "stk_1": {
                            "seat": 0,
                            "zone": "stack",
                            "zoneIndex": 0,
                            "oracleId": "synth_spell_1",
                            "initialCardOrdinal": 8
                        },
                        "crd_2": {
                            "ownerSeat": 0,
                            "zone": "battlefield",
                            "ordinal": 0,
                            "oracleId": "synth_creature_0"
                        },
                        "crd_3": {
                            "initialCardOrdinal": 12,
                            "seat": 0,
                            "oracleId": "synth_land_0"
                        },
                        "tok_1": {
                            "tokenSeq": 0,
                            "seat": 0,
                            "oracleId": "tok_beast"
                        }
                    },
                    "digestAfter": {
                        "turnNumber": 1,
                        "activeSeat": 0,
                        "prioritySeat": 1,
                        "phase": "main1",
                        "stackDepth": 1,
                        "pendingPromptKind": null,
                        "players": [sample_player_digest(0)]
                    }
                }
            ]
        });

        let parsed: TsReplayTrace =
            serde_json::from_value(trace_json).expect("full TsReplayTrace must deserialize");
        assert_eq!(parsed.steps[0].digest_after.phase, TsReplayPhase::Mulligan);
        assert_eq!(parsed.steps[1].digest_after.phase, TsReplayPhase::Main1);

        let locators = &parsed.steps[1].object_locators;
        assert_eq!(
            locators.get("crd_1"),
            Some(&ObjectLocator::ZoneSlot(ZoneSlotLocator {
                seat: 0,
                zone: Zone::Hand,
                zone_index: 1,
                oracle_id: "synth_spell_1".to_string(),
                initial_card_ordinal: None,
                token_seq: None,
            }))
        );
        assert_eq!(
            locators.get("stk_1"),
            Some(&ObjectLocator::ZoneSlot(ZoneSlotLocator {
                seat: 0,
                zone: Zone::Stack,
                zone_index: 0,
                oracle_id: "synth_spell_1".to_string(),
                initial_card_ordinal: Some(8),
                token_seq: None,
            }))
        );
        assert_eq!(
            locators.get("crd_2"),
            Some(&ObjectLocator::OwnerOrdinal(OwnerOrdinalLocator {
                owner_seat: 0,
                zone: Zone::Battlefield,
                ordinal: 0,
                oracle_id: "synth_creature_0".to_string(),
            }))
        );
        assert_eq!(
            locators.get("crd_3"),
            Some(&ObjectLocator::InitialCard(InitialCardLocator {
                initial_card_ordinal: 12,
                token_seq: None,
                seat: Some(0),
                oracle_id: Some("synth_land_0".to_string()),
            }))
        );
        assert_eq!(
            locators.get("tok_1"),
            Some(&ObjectLocator::Token(TokenLocator {
                token_seq: 0,
                initial_card_ordinal: None,
                seat: Some(0),
                oracle_id: Some("tok_beast".to_string()),
            }))
        );

        let re_json = serde_json::to_string(&parsed).expect("serialize TsReplayTrace");
        let reparsed: TsReplayTrace =
            serde_json::from_str(&re_json).expect("re-deserialize TsReplayTrace");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn ts_replay_structs_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<TsReplayRules>(serde_json::json!({
                "startingLife": 20,
                "commander": false,
                "freeFirstMulligan": false,
                "extra": 1
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsReplaySeat>(serde_json::json!({
                "seat": 0,
                "deckId": "d",
                "commanders": [],
                "libraryOrderInitial": [],
                "handInitial": [],
                "extra": true
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsZoneCounts>(serde_json::json!({
                "library": 53,
                "hand": 7,
                "battlefield": 0,
                "graveyard": 0,
                "exile": 0,
                "command": 0,
                "stack": 0
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsBattlefieldEntryDigest>(serde_json::json!({
                "oracleId": "c",
                "tapped": false,
                "pt": [1, 1],
                "types": ["Creature"],
                "keywords": [],
                "counters": [],
                "damageMarked": 0,
                "attachedToOracleId": null,
                "extra": "nope"
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsPlayerDigest>(serde_json::json!({
                "seat": 0,
                "life": 20,
                "lost": false,
                "poison": 0,
                "energy": 0,
                "manaPool": [0, 0, 0, 0, 0, 0],
                "zoneCounts": {
                    "library": 53,
                    "hand": 7,
                    "battlefield": 0,
                    "graveyard": 0,
                    "exile": 0,
                    "command": 0
                },
                "handOracleIds": [],
                "battlefield": [],
                "extra": 42
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsStepDigest>(serde_json::json!({
                "turnNumber": 1,
                "activeSeat": 0,
                "prioritySeat": 0,
                "phase": "main1",
                "stackDepth": 0,
                "pendingPromptKind": null,
                "players": [],
                "extra": "bad"
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsReplayStep>(serde_json::json!({
                "stepIndex": 0,
                "actorSeat": 0,
                "action": { "type": "pass_priority" },
                "objectLocators": {},
                "digestAfter": {
                    "turnNumber": 1,
                    "activeSeat": 0,
                    "prioritySeat": 0,
                    "phase": "main1",
                    "stackDepth": 0,
                    "pendingPromptKind": null,
                    "players": []
                },
                "rngStateBefore": 123
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<TsReplayTrace>(serde_json::json!({
                "formatVersion": 1,
                "compilerVersion": 46,
                "seed": 42,
                "postInitRngState": 42,
                "rules": {
                    "startingLife": 20,
                    "commander": false,
                    "freeFirstMulligan": false
                },
                "seats": [],
                "steps": [],
                "unexpected": 99
            }))
            .is_err()
        );

        for bad_locator in [
            serde_json::json!({ "seat": 0, "zone": "hand", "zoneIndex": 0, "oracleId": "c", "extra": 1 }),
            serde_json::json!({ "ownerSeat": 0, "zone": "hand", "ordinal": 0, "oracleId": "c", "extra": 1 }),
            serde_json::json!({ "initialCardOrdinal": 0, "extra": 1 }),
            serde_json::json!({ "tokenSeq": 0, "extra": 1 }),
        ] {
            assert!(
                serde_json::from_value::<ObjectLocator>(bad_locator.clone()).is_err(),
                "locator with unknown field must fail: {bad_locator}"
            );
        }
    }

    #[test]
    fn object_locator_rejects_empty_and_null_identity_objects() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "initialCardOrdinal": null, "tokenSeq": null }),
            serde_json::json!({ "initialCardOrdinal": null }),
            serde_json::json!({ "tokenSeq": null }),
            serde_json::json!({ "seat": 0, "oracleId": "card_only" }),
        ] {
            assert!(
                serde_json::from_value::<ObjectLocator>(bad.clone()).is_err(),
                "ambiguous/empty locator must be rejected: {bad}"
            );
        }
    }

    #[test]
    fn ts_replay_phase_mulligan_and_game_phase_conversion() {
        let mulligan: TsReplayPhase =
            serde_json::from_value(serde_json::json!("mulligan")).unwrap();
        assert_eq!(mulligan, TsReplayPhase::Mulligan);
        assert_eq!(mulligan.as_game_phase(), None);

        let all_game_phases = [
            GamePhase::Untap,
            GamePhase::Upkeep,
            GamePhase::Draw,
            GamePhase::Main1,
            GamePhase::BeginCombat,
            GamePhase::DeclareAttackers,
            GamePhase::DeclareBlockers,
            GamePhase::FirstStrikeDamage,
            GamePhase::CombatDamage,
            GamePhase::EndCombat,
            GamePhase::Main2,
            GamePhase::EndStep,
            GamePhase::Cleanup,
        ];
        for gp in all_game_phases {
            let rp = TsReplayPhase::from(gp);
            assert_eq!(rp.as_game_phase(), Some(gp));
            let serialized = serde_json::to_value(rp).unwrap();
            assert_eq!(serialized, serde_json::to_value(gp).unwrap());
            let decoded: TsReplayPhase = serde_json::from_value(serialized).unwrap();
            assert_eq!(decoded, rp);
        }

        assert!(serde_json::from_value::<TsReplayPhase>(serde_json::json!("combat")).is_err());
    }

    #[test]
    fn ts_battlefield_entry_digest_supports_negative_unclamped_and_null_pt() {
        for pt_val in [
            serde_json::json!([-3, -1]),
            serde_json::json!([4, -2]),
            serde_json::json!([0, 0]),
            serde_json::json!(null),
        ] {
            let entry_json = serde_json::json!({
                "oracleId": "synth_card",
                "tapped": false,
                "pt": pt_val,
                "types": ["Creature"],
                "keywords": [],
                "counters": [],
                "damageMarked": 0,
                "attachedToOracleId": null
            });
            let parsed: TsBattlefieldEntryDigest = serde_json::from_value(entry_json.clone())
                .expect("unclamped or null pt must deserialize");
            let re_val = serde_json::to_value(&parsed).unwrap();
            assert_eq!(re_val["pt"], entry_json["pt"]);
        }
    }
}
