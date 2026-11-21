use std::sync::Arc;

use serde::ser::{SerializeSeq, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use smallvec::SmallVec;

/// 0-based seat / player index (`0..player_count`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlayerId(pub u8);

/// Generational arena handle for every MTG object across all zones (including the stack).
///
/// Bumping `generation` on cross-zone transitions naturally enforces CR 400.7 ("an object that
/// moves from one zone to another becomes a new object with no memory of its previous existence").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectId {
    pub index: u32,
    pub generation: u32,
}

/// Identifier for an item on the stack (spell or activated/triggered ability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StackItemId(pub u32);

const CHUNK_SIZE: usize = 64;
const CHUNK_MASK: usize = CHUNK_SIZE - 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

#[derive(Debug, Clone)]
struct ArenaChunk<T> {
    slots: [Slot<T>; CHUNK_SIZE],
}

impl<T> ArenaChunk<T> {
    #[inline]
    fn new_empty() -> Self {
        Self {
            slots: std::array::from_fn(|_| Slot {
                generation: 0,
                value: None,
            }),
        }
    }
}

/// Compact generational arena holding all [`crate::GameObject`]s in a match.
///
/// Stores slots in 64-entry copy-on-write (`Arc`) chunks so `GameState::clone()` only bumps
/// chunk refcounts while preserving O(1) lookup and the `(slots, free_list, len)` wire format.
#[derive(Debug)]
pub struct ObjectArena<T> {
    chunks: SmallVec<[Arc<ArenaChunk<T>>; 8]>,
    free_list: Vec<u32>,
    slot_len: usize,
    len: usize,
}

impl<T> Default for ObjectArena<T> {
    fn default() -> Self {
        Self {
            chunks: SmallVec::new(),
            free_list: Vec::new(),
            slot_len: 0,
            len: 0,
        }
    }
}

impl<T> Clone for ObjectArena<T> {
    fn clone(&self) -> Self {
        Self {
            chunks: self.chunks.clone(),
            free_list: self.free_list.clone(),
            slot_len: self.slot_len,
            len: self.len,
        }
    }
}

struct SlotsSeq<'a, T> {
    chunks: &'a [Arc<ArenaChunk<T>>],
    slot_len: usize,
}

impl<T: Serialize> Serialize for SlotsSeq<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.slot_len))?;
        let mut remaining = self.slot_len;
        for chunk in self.chunks {
            let take = remaining.min(CHUNK_SIZE);
            for slot in &chunk.slots[..take] {
                seq.serialize_element(slot)?;
            }
            remaining -= take;
            if remaining == 0 {
                break;
            }
        }
        seq.end()
    }
}

impl<T: Serialize> Serialize for ObjectArena<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("ObjectArena", 3)?;
        st.serialize_field(
            "slots",
            &SlotsSeq {
                chunks: &self.chunks,
                slot_len: self.slot_len,
            },
        )?;
        st.serialize_field("free_list", &self.free_list)?;
        st.serialize_field("len", &self.len)?;
        st.end()
    }
}

#[derive(Deserialize)]
#[serde(rename = "ObjectArena")]
struct ObjectArenaWire<T> {
    slots: Vec<Slot<T>>,
    free_list: Vec<u32>,
    len: usize,
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectArena<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ObjectArenaWire::<T>::deserialize(deserializer)?;
        let slot_len = wire.slots.len();
        let num_chunks = slot_len.div_ceil(CHUNK_SIZE);
        let mut chunks: SmallVec<[Arc<ArenaChunk<T>>; 8]> = SmallVec::with_capacity(num_chunks);
        let mut iter = wire.slots.into_iter();
        for c_idx in 0..num_chunks {
            let take = (slot_len - c_idx * CHUNK_SIZE).min(CHUNK_SIZE);
            let mut chunk = ArenaChunk::new_empty();
            for (dst, slot) in chunk.slots[..take].iter_mut().zip(iter.by_ref()) {
                *dst = slot;
            }
            chunks.push(Arc::new(chunk));
        }
        Ok(Self {
            chunks,
            free_list: wire.free_list,
            slot_len,
            len: wire.len,
        })
    }
}

impl<T> ObjectArena<T> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            chunks: SmallVec::with_capacity(cap.div_ceil(CHUNK_SIZE)),
            free_list: Vec::new(),
            slot_len: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn get(&self, id: ObjectId) -> Option<&T> {
        let idx = id.index as usize;
        if idx >= self.slot_len {
            return None;
        }
        let chunk = self.chunks.get(idx >> 6)?;
        let slot = &chunk.slots[idx & CHUNK_MASK];
        if slot.generation == id.generation {
            slot.value.as_ref()
        } else {
            None
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn slot_capacity(&self) -> usize {
        self.slot_len
    }

    pub fn iter(&self) -> impl Iterator<Item = (ObjectId, &T)> {
        let slot_len = self.slot_len;
        self.chunks
            .iter()
            .enumerate()
            .flat_map(|(c_idx, chunk)| {
                let base = c_idx * CHUNK_SIZE;
                chunk
                    .slots
                    .iter()
                    .enumerate()
                    .map(move |(s_idx, s)| (base + s_idx, s))
            })
            .take(slot_len)
            .filter_map(|(i, s)| {
                s.value.as_ref().map(|v| {
                    (
                        ObjectId {
                            index: i as u32,
                            generation: s.generation,
                        },
                        v,
                    )
                })
            })
    }
}

impl<T: Clone> ObjectArena<T> {
    pub fn insert(&mut self, value: T) -> ObjectId {
        self.len += 1;
        if let Some(index) = self.free_list.pop() {
            let idx = index as usize;
            let chunk_idx = idx >> 6;
            let slot_idx = idx & CHUNK_MASK;
            let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
            let slot = &mut chunk.slots[slot_idx];
            slot.generation = slot.generation.wrapping_add(1).max(1);
            slot.value = Some(value);
            ObjectId {
                index,
                generation: slot.generation,
            }
        } else {
            let idx = self.slot_len;
            let index = idx as u32;
            let chunk_idx = idx >> 6;
            let slot_idx = idx & CHUNK_MASK;
            if chunk_idx == self.chunks.len() {
                self.chunks.push(Arc::new(ArenaChunk::new_empty()));
            }
            let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
            chunk.slots[slot_idx] = Slot {
                generation: 1,
                value: Some(value),
            };
            self.slot_len += 1;
            ObjectId {
                index,
                generation: 1,
            }
        }
    }

    #[inline]
    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut T> {
        let idx = id.index as usize;
        if idx >= self.slot_len {
            return None;
        }
        let chunk_idx = idx >> 6;
        let slot_idx = idx & CHUNK_MASK;
        let ro = self.chunks.get(chunk_idx)?;
        let ro_slot = &ro.slots[slot_idx];
        if ro_slot.generation != id.generation || ro_slot.value.is_none() {
            return None;
        }
        let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
        chunk.slots[slot_idx].value.as_mut()
    }

    /// Invalidates `id` and returns a fresh `ObjectId` pointing to the same underlying slot
    /// with an incremented generation (CR 400.7 zone change identity refresh).
    pub fn bump_generation(&mut self, id: ObjectId) -> Option<ObjectId> {
        let idx = id.index as usize;
        if idx >= self.slot_len {
            return None;
        }
        let chunk_idx = idx >> 6;
        let slot_idx = idx & CHUNK_MASK;
        let ro = self.chunks.get(chunk_idx)?;
        let ro_slot = &ro.slots[slot_idx];
        if ro_slot.generation != id.generation || ro_slot.value.is_none() {
            return None;
        }
        let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
        let slot = &mut chunk.slots[slot_idx];
        slot.generation = slot.generation.wrapping_add(1).max(1);
        Some(ObjectId {
            index: id.index,
            generation: slot.generation,
        })
    }

    pub fn remove(&mut self, id: ObjectId) -> Option<T> {
        let idx = id.index as usize;
        if idx >= self.slot_len {
            return None;
        }
        let chunk_idx = idx >> 6;
        let slot_idx = idx & CHUNK_MASK;
        let ro = self.chunks.get(chunk_idx)?;
        let ro_slot = &ro.slots[slot_idx];
        if ro_slot.generation != id.generation || ro_slot.value.is_none() {
            return None;
        }
        let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
        let slot = &mut chunk.slots[slot_idx];
        let val = slot.value.take()?;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free_list.push(id.index);
        self.len -= 1;
        Some(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cow64_arena_clone_aliasing_multi_chunk_isolation_and_serde_parity() {
        let mut arena = ObjectArena::with_capacity(150);
        let mut ids = Vec::with_capacity(150);
        for i in 0u32..150 {
            ids.push(arena.insert(i * 10));
        }
        assert_eq!(arena.len(), 150);
        assert_eq!(arena.slot_capacity(), 150);

        // Remove an item in chunk 0 and bump an item in chunk 1, leaving chunk 2 untouched.
        assert_eq!(arena.remove(ids[5]), Some(50));
        assert_eq!(arena.get(ids[5]), None);
        let bumped_id = arena.bump_generation(ids[70]).unwrap();
        assert_eq!(arena.get(ids[70]), None);
        assert_eq!(arena.get(bumped_id), Some(&700));
        assert_eq!(arena.len(), 149);

        // Clone shares chunks via Arc; mutating clone must not affect original.
        let mut cloned = arena.clone();
        assert_eq!(cloned.chunks.len(), 3);
        assert!(Arc::ptr_eq(&arena.chunks[0], &cloned.chunks[0]));
        assert!(Arc::ptr_eq(&arena.chunks[1], &cloned.chunks[1]));
        assert!(Arc::ptr_eq(&arena.chunks[2], &cloned.chunks[2]));

        // Mutate chunk 0 (reuse freed slot 5) and chunk 2 (get_mut slot 140) on clone.
        let reinserted = cloned.insert(9999);
        assert_eq!(reinserted.index, 5);
        *cloned.get_mut(ids[140]).unwrap() = 8888;
        assert_eq!(cloned.remove(ids[141]), Some(1410));

        // Chunk 1 remains shared; chunks 0 and 2 diverged cleanly.
        assert!(!Arc::ptr_eq(&arena.chunks[0], &cloned.chunks[0]));
        assert!(Arc::ptr_eq(&arena.chunks[1], &cloned.chunks[1]));
        assert!(!Arc::ptr_eq(&arena.chunks[2], &cloned.chunks[2]));

        // Original arena is completely intact.
        assert_eq!(arena.len(), 149);
        assert_eq!(arena.get(ids[5]), None);
        assert_eq!(arena.get(reinserted), None);
        assert_eq!(arena.get(ids[140]), Some(&1400));
        assert_eq!(arena.get(ids[141]), Some(&1410));

        // Postcard and JSON wire roundtrip preserves slots, free_list, len, and exact iter order.
        let json_val = serde_json::to_value(&arena).unwrap();
        assert!(json_val.get("slots").is_some());
        assert!(json_val.get("free_list").is_some());
        assert!(json_val.get("len").is_some());
        let decoded_json: ObjectArena<u32> = serde_json::from_value(json_val).unwrap();

        let pc_bytes = postcard::to_allocvec(&arena).unwrap();
        let decoded_pc: ObjectArena<u32> = postcard::from_bytes(&pc_bytes).unwrap();

        let orig_items: Vec<(ObjectId, u32)> = arena.iter().map(|(id, &v)| (id, v)).collect();
        let json_items: Vec<(ObjectId, u32)> =
            decoded_json.iter().map(|(id, &v)| (id, v)).collect();
        let pc_items: Vec<(ObjectId, u32)> = decoded_pc.iter().map(|(id, &v)| (id, v)).collect();
        assert_eq!(orig_items, json_items);
        assert_eq!(orig_items, pc_items);
        assert_eq!(decoded_pc.len(), arena.len());
        assert_eq!(decoded_pc.slot_capacity(), arena.slot_capacity());
    }
}
