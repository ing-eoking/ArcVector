//! Both directions of `id <-> u64`, with the reverse direction stored by slot rather than
//! hashed.
//!
//! A search hands back keys and nothing else can turn one into an id, so `key -> id` has to be
//! in this process; keeping `id -> key` beside it means a write learns the node it displaces
//! from a lookup here instead of an engine read, which keeps the engine out of the hold that
//! orders writes.
//!
//! The key is `(slot << 32) | generation`, so the reverse direction is an array index rather
//! than a hash — measured at 6.9ns against 45.0ns, on the hottest read there is. The
//! generation is what keeps a recycled slot from answering an old key: a slot is reused, a key
//! never is. Without it a stale key captured before a delete would resolve to whatever took
//! the slot next, which is a wrong id in a reply rather than a missing row.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Bits of the key that name the slot; the rest is the generation.
const SLOT_SHIFT: u32 = 32;

pub(super) const fn slot_of(key: u64) -> usize {
    (key >> SLOT_SHIFT) as usize
}

pub(super) const fn generation_of(key: u64) -> u32 {
    key as u32
}

pub(super) const fn key_of(slot: u32, generation: u32) -> u64 {
    ((slot as u64) << SLOT_SHIFT) | generation as u64
}

/// One array position. `generation` is the only one of its keys that answers.
#[derive(Default, Clone)]
pub(super) struct Slot {
    generation: u32,
    id: Option<Arc<str>>,
}

#[derive(Default)]
pub(super) struct IdMap {
    slots: Vec<Slot>,
    by_id: HashMap<Arc<str>, u64>,
    /// Ids deleted while a rebuild is running.
    pub(super) tombstones: HashSet<Arc<str>>,
    /// Total length of the live id strings, kept as a running sum.
    id_bytes: usize,
}

impl IdMap {
    /// Name `key` as `id`, and report the key `id` named before — whose node nothing can
    /// reach now, so the caller detaches it and hands the slot back.
    ///
    /// No winner is decided here. Every caller holds this map's write lock across the element
    /// write too, so writes to one id arrive in one order.
    pub(super) fn bind(&mut self, id: &str, key: u64) -> Option<u64> {
        let slot = slot_of(key);
        if slot >= self.slots.len() {
            self.slots.resize(slot + 1, Slot::default());
        }

        let displaced = match self.by_id.get_mut(id) {
            Some(held) => Some(std::mem::replace(held, key)),
            None => {
                let text: Arc<str> = Arc::from(id);
                self.id_bytes += id.len();
                self.by_id.insert(Arc::clone(&text), key);
                None
            }
        };
        // Reuse the text the map already holds, so an overwrite allocates nothing.
        let text = self
            .by_id
            .get_key_value(id)
            .map(|(text, _)| Arc::clone(text))
            .expect("just inserted or already present");

        if let Some(previous) = displaced {
            self.slots[slot_of(previous)].id = None;
        }
        self.slots[slot] = Slot {
            generation: generation_of(key),
            id: Some(text),
        };
        displaced
    }

    /// Stop naming `id`, reporting the key it named so the caller can detach that node.
    pub(super) fn forget(&mut self, id: &str) -> Option<u64> {
        let (text, key) = self.by_id.remove_entry(id)?;
        self.id_bytes -= text.len();
        self.slots[slot_of(key)].id = None;
        self.tombstones.remove(id);
        Some(key)
    }

    /// Forget `id`, but remember that it was deleted so a refill cannot replay it.
    pub(super) fn forget_tombstoned(&mut self, id: &str) -> Option<u64> {
        let key = self.forget(id);
        self.tombstones.insert(Arc::from(id));
        key
    }

    /// The id this exact key names — the generation has to match, or the key is one the slot
    /// answered before it was recycled.
    pub(super) fn id_of(&self, key: u64) -> Option<&Arc<str>> {
        let slot = self.slots.get(slot_of(key))?;
        if slot.generation != generation_of(key) {
            return None;
        }
        slot.id.as_ref()
    }

    /// Whether a rebuild should leave this id alone: the graph already holds it, or it was
    /// deleted since the rebuild began.
    pub(super) fn is_known(&self, id: &str) -> bool {
        self.by_id.contains_key(id) || self.tombstones.contains(id)
    }

    /// Put a retired slot back in service at generation 0.
    ///
    /// Only safe with no search in flight: a key from before the slot retired would match
    /// generation 0 again. The caller owns that check.
    pub(super) fn revive(&mut self, slot: u32) {
        if let Some(held) = self.slots.get_mut(slot as usize) {
            *held = Slot::default();
        }
    }

    pub(super) fn len(&self) -> usize {
        self.by_id.len()
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl Iterator<Item = (u64, &Arc<str>)> {
        self.by_id.iter().map(|(text, key)| (*key, text))
    }

    pub(super) fn bytes(&self) -> usize {
        const ARC_HEADER: usize = 16; // strong + weak counts, one per shared id text
        let per_id = size_of::<Arc<str>>() + size_of::<u64>() + 1;
        self.slots.capacity() * size_of::<Slot>()
            + (self.by_id.len() * 8).div_ceil(7) * per_id
            + self.by_id.len() * ARC_HEADER
            + self.id_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_a_replacement_reports_the_displaced_key_and_counts_the_text_once() {
        let mut ids = IdMap::default();
        assert_eq!(
            ids.bind("v1", key_of(0, 0)),
            None,
            "a new id displaces nothing"
        );
        let after_first = ids.bytes();

        let displaced = ids.bind("v1", key_of(1, 0));
        assert_eq!(displaced, Some(key_of(0, 0)), "the key v1 used to name");
        assert_eq!(ids.len(), 1);
        assert_eq!(ids.id_of(key_of(1, 0)).map(|s| &**s), Some("v1"));
        assert_eq!(ids.id_of(key_of(0, 0)), None, "the displaced slot is empty");
        assert_eq!(ids.bytes(), after_first, "id_bytes counted the text once");
    }

    #[test]
    fn a_recycled_slot_does_not_answer_the_key_it_answered_before() {
        let mut ids = IdMap::default();
        ids.bind("v1", key_of(7, 0));
        ids.forget("v1");

        // The slot comes back with its generation advanced, as `drop_node` hands it back.
        ids.bind("v9", key_of(7, 1));

        assert_eq!(ids.id_of(key_of(7, 1)).map(|s| &**s), Some("v9"));
        assert_eq!(
            ids.id_of(key_of(7, 0)),
            None,
            "the stale key is a miss, not somebody else's id"
        );
    }

    #[test]
    fn forgetting_reports_the_key_and_releases_the_text() {
        let mut ids = IdMap::default();
        ids.bind("v1", key_of(3, 5));
        assert_eq!(ids.forget("v1"), Some(key_of(3, 5)));
        assert_eq!(ids.id_of(key_of(3, 5)), None);
        assert_eq!(ids.bytes(), ids.slots.capacity() * size_of::<Slot>());
        assert_eq!(ids.forget("v1"), None, "forgetting twice is not an error");
    }

    #[test]
    fn a_tombstone_outlives_the_binding() {
        let mut ids = IdMap::default();
        ids.bind("v1", key_of(2, 0));
        assert_eq!(ids.forget_tombstoned("v1"), Some(key_of(2, 0)));

        assert_eq!(ids.len(), 0);
        assert!(ids.is_known("v1"), "a refill must not replay a delete");
    }

    #[test]
    fn binding_again_clears_the_tombstone() {
        let mut ids = IdMap::default();
        ids.forget_tombstoned("v1");
        assert!(ids.is_known("v1"));

        ids.bind("v1", key_of(4, 0));
        assert_eq!(ids.forget("v1"), Some(key_of(4, 0)));
        assert!(!ids.is_known("v1"), "the delete was undone by a live write");
    }

    #[test]
    fn reviving_a_slot_returns_it_to_generation_zero() {
        let mut ids = IdMap::default();
        ids.bind("v1", key_of(6, u32::MAX));
        ids.forget("v1");
        ids.revive(6);

        ids.bind("v9", key_of(6, 0));
        assert_eq!(ids.id_of(key_of(6, 0)).map(|s| &**s), Some("v9"));
        assert_eq!(
            ids.id_of(key_of(6, u32::MAX)),
            None,
            "the generation that retired the slot is not answered again"
        );
    }

    #[test]
    fn the_key_splits_into_slot_and_generation() {
        let key = key_of(0x1234_5678, 0x9abc_def0);
        assert_eq!(slot_of(key), 0x1234_5678);
        assert_eq!(generation_of(key), 0x9abc_def0);
    }

    /// What the slot array costs and what it buys, on the real structure. Run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn slot_lookup_against_a_hash_map() {
        use std::collections::HashMap;
        use std::time::Instant;
        const N: usize = 2_000_000;

        let ids: Vec<String> = (0..N).map(|i| format!("doc:{i:09}")).collect();

        let mut slots = IdMap::default();
        let mut hashed: HashMap<u64, Arc<str>> = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            slots.bind(id, key_of(i as u32, 0));
            hashed.insert(key_of(i as u32, 0), Arc::from(id.as_str()));
        }

        let probe: Vec<u64> = (0..N as u32).step_by(7).map(|i| key_of(i, 0)).collect();
        let mut seen = 0usize;

        let t = Instant::now();
        for k in &probe {
            seen += slots.id_of(*k).is_some() as usize;
        }
        let by_slot = t.elapsed();

        let t = Instant::now();
        for k in &probe {
            seen += hashed.contains_key(k) as usize;
        }
        let by_hash = t.elapsed();

        println!(
            "n={N}  slot {:.1}ns/op  hash {:.1}ns/op  ({:.1}x)   idmap {} MB",
            by_slot.as_nanos() as f64 / probe.len() as f64,
            by_hash.as_nanos() as f64 / probe.len() as f64,
            by_hash.as_nanos() as f64 / by_slot.as_nanos() as f64,
            slots.bytes() / 1_000_000,
        );
        std::hint::black_box(seen);
    }
}
