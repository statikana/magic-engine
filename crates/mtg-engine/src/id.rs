use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// Compact generational arena holding all [`crate::GameObject`]s in a match.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObjectArena<T> {
    slots: Vec<Slot<T>>,
    free_list: Vec<u32>,
    len: usize,
}

impl<T> ObjectArena<T> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            slots: Vec::with_capacity(cap),
            free_list: Vec::new(),
            len: 0,
        }
    }

    pub fn insert(&mut self, value: T) -> ObjectId {
        self.len += 1;
        if let Some(index) = self.free_list.pop() {
            let slot = &mut self.slots[index as usize];
            slot.generation = slot.generation.wrapping_add(1).max(1);
            slot.value = Some(value);
            ObjectId {
                index,
                generation: slot.generation,
            }
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Slot {
                generation: 1,
                value: Some(value),
            });
            ObjectId {
                index,
                generation: 1,
            }
        }
    }

    #[inline]
    pub fn get(&self, id: ObjectId) -> Option<&T> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation == id.generation {
            slot.value.as_ref()
        } else {
            None
        }
    }

    #[inline]
    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation == id.generation {
            slot.value.as_mut()
        } else {
            None
        }
    }

    /// Invalidates `id` and returns a fresh `ObjectId` pointing to the same underlying slot
    /// with an incremented generation (CR 400.7 zone change identity refresh).
    pub fn bump_generation(&mut self, id: ObjectId) -> Option<ObjectId> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation || slot.value.is_none() {
            return None;
        }
        slot.generation = slot.generation.wrapping_add(1).max(1);
        Some(ObjectId {
            index: id.index,
            generation: slot.generation,
        })
    }

    pub fn remove(&mut self, id: ObjectId) -> Option<T> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation != id.generation {
            return None;
        }
        let val = slot.value.take()?;
        slot.generation = slot.generation.wrapping_add(1).max(1);
        self.free_list.push(id.index);
        self.len -= 1;
        Some(val)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = (ObjectId, &T)> {
        self.slots.iter().enumerate().filter_map(|(i, s)| {
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
