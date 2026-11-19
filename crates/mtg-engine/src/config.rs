use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MulliganRule {
    London { free_first: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RngMode {
    /// Exact compatibility with TS `runtime/random.ts` (`Mulberry32`) for differential testing.
    Mulberry32Compat,
    /// Fast 64-bit PRNG for native play and ISMCTS rollouts.
    Fast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommanderRules {
    pub commander_damage_threshold: i32,
    pub commander_tax_generic: u16,
    pub enforce_color_identity: bool,
    pub singleton: bool,
    pub deck_size: u16,
}

impl Default for CommanderRules {
    fn default() -> Self {
        Self {
            commander_damage_threshold: 21,
            commander_tax_generic: 2,
            enforce_color_identity: true,
            singleton: true,
            deck_size: 100,
        }
    }
}

/// Format-agnostic game configuration (1v1, Multiplayer, Commander CR 903, Limited, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameConfig {
    pub player_count: u8,
    pub starting_life: i32,
    pub starting_hand_size: u8,
    pub max_hand_size: u8,
    pub mulligan: MulliganRule,
    pub commander: Option<CommanderRules>,
    /// Enables optional manual guided resolution for unsupported abilities in casual web tables.
    /// Must be `false` for strict, tournament, and AI training modes.
    pub allow_guided_fallback: bool,
    pub rng_mode: RngMode,
}

impl GameConfig {
    pub fn standard_1v1() -> Self {
        Self {
            player_count: 2,
            starting_life: 20,
            starting_hand_size: 7,
            max_hand_size: 7,
            mulligan: MulliganRule::London { free_first: false },
            commander: None,
            allow_guided_fallback: false,
            rng_mode: RngMode::Fast,
        }
    }

    pub fn commander(player_count: u8) -> Self {
        Self {
            player_count: player_count.clamp(2, 8),
            starting_life: 40,
            starting_hand_size: 7,
            max_hand_size: 7,
            mulligan: MulliganRule::London { free_first: true },
            commander: Some(CommanderRules::default()),
            allow_guided_fallback: false,
            rng_mode: RngMode::Fast,
        }
    }
}
