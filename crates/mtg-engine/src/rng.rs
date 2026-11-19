use serde::{Deserialize, Serialize};

/// Deterministic PRNG state embedded in [`crate::GameState`].
///
/// Supports both exact TS-compatible `Mulberry32` (for bit-for-bit differential replay testing
/// against `peckhamr.com/src/lib/magic/runtime/random.ts`) and a fast 64-bit `SplitMix64`
/// generator for normal play and ISMCTS rollouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RngState {
    Mulberry32 { state: u32 },
    SplitMix64 { state: u64 },
}

impl RngState {
    pub fn mulberry32(seed: u32) -> Self {
        Self::Mulberry32 { state: seed }
    }

    pub fn splitmix64(seed: u64) -> Self {
        Self::SplitMix64 { state: seed }
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        match self {
            Self::Mulberry32 { state } => {
                *state = state.wrapping_add(0x6d2b_79f5);
                let mut t = *state;
                t = (t ^ (t >> 15)).wrapping_mul(t | 1);
                t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
                t ^ (t >> 14)
            }
            Self::SplitMix64 { state } => (next_splitmix64(state) >> 32) as u32,
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        match self {
            Self::Mulberry32 { .. } => {
                let hi = u64::from(self.next_u32());
                let lo = u64::from(self.next_u32());
                (hi << 32) | lo
            }
            Self::SplitMix64 { state } => next_splitmix64(state),
        }
    }

    /// Returns a uniform index in `0..bound`.
    ///
    /// For `Mulberry32`, matches TS `Math.floor(nextRandom(holder) * bound)` bit-for-bit.
    #[inline]
    pub fn next_index(&mut self, bound: usize) -> usize {
        debug_assert!(bound > 0);
        match self {
            Self::Mulberry32 { .. } => {
                let f = (self.next_u32() as f64) / 4_294_967_296.0;
                (f * (bound as f64)).floor() as usize
            }
            Self::SplitMix64 { .. } => (self.next_u64() as usize) % bound,
        }
    }

    /// Fisher-Yates shuffle matching TS `shuffleWith` in `runtime/random.ts`.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.next_index(i + 1);
            items.swap(i, j);
        }
    }
}

#[inline]
fn next_splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}
