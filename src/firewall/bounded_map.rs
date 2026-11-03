//! Fixed-address, bounded scoped-state table for attacker-driven keys.
//!
//! Hashing and eight-slot replacement windows never change as the table fills.
//! Slot storage is reserved only on first write; an occupancy bitmap protects
//! uninitialized payload and prevents touching empty pages. Retain releases
//! empty segments. The configured
//! slot bound and earliest-expiry replacement policy remain in force.

use ahash::RandomState;
use parking_lot::Mutex;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};

const PROBES: usize = 8;
const SEGMENTS: usize = 64;

struct SlotStorage<K> {
    // Only occupied bits permit reads. K: Copy excludes drop glue, so stale
    // values after removal need no destruction. The segment mutex publishes
    // writes before occupancy and excludes concurrent readers during removal.
    slots: Box<[std::mem::MaybeUninit<(K, i64)>]>,
    occupied: Box<[u64]>,
}

impl<K: Copy> SlotStorage<K> {
    fn new(capacity: usize) -> Self {
        Self {
            // Reserve virtual capacity without writing every payload page.
            slots: Box::<[(K, i64)]>::new_uninit_slice(capacity),
            occupied: vec![0; capacity.div_ceil(64)].into_boxed_slice(),
        }
    }

    #[inline]
    fn get(&self, index: usize) -> Option<(K, i64)> {
        if self.occupied[index / 64] & (1 << (index % 64)) == 0 {
            return None;
        }
        // SAFETY: put writes the complete Copy value before setting its bit;
        // all access holds the same segment mutex, and the index is in bounds.
        Some(unsafe { self.slots[index].assume_init() })
    }

    #[inline]
    fn put(&mut self, index: usize, value: Option<(K, i64)>) {
        let mask = 1 << (index % 64);
        if let Some(value) = value {
            self.slots[index].write(value);
            self.occupied[index / 64] |= mask;
        } else {
            self.occupied[index / 64] &= !mask;
        }
    }

    fn entries(&self) -> impl Iterator<Item = (usize, K, i64)> + '_ {
        self.occupied
            .iter()
            .enumerate()
            .flat_map(move |(word, &mask)| {
                let mut remaining = mask;
                std::iter::from_fn(move || {
                    if remaining == 0 {
                        return None;
                    }
                    let bit = remaining.trailing_zeros() as usize;
                    remaining &= remaining - 1;
                    let index = word * 64 + bit;
                    // SAFETY: only bits published by put are enumerated. Padding
                    // bits in the final word are never set; values are Copy.
                    let (key, expiry) = unsafe { self.slots[index].assume_init() };
                    Some((index, key, expiry))
                })
            })
    }
}

struct Segment<K> {
    storage: Option<SlotStorage<K>>,
    len: usize,
}

impl<K: Copy> Segment<K> {
    #[inline]
    fn slot(&self, index: usize) -> Option<(K, i64)> {
        self.storage.as_ref()?.get(index)
    }

    #[inline]
    fn put(&mut self, index: usize, value: Option<(K, i64)>, capacity: usize) {
        if value.is_none() && self.storage.is_none() {
            return;
        }
        self.storage
            .get_or_insert_with(|| SlotStorage::new(capacity))
            .put(index, value);
    }

    fn earliest(&self) -> Option<usize> {
        self.storage
            .as_ref()?
            .entries()
            .min_by_key(|(_, _, expiry)| *expiry)
            .map(|(index, _, _)| index)
    }
}

/// Bounded map with immutable slot addresses. Lookups and ordinary inserts
/// probe eight slots. Cold iteration scans occupancy words and live values, not empty payload.
pub struct BoundedScopedMap<K> {
    segs: Box<[Mutex<Segment<K>>]>,
    seg_cap: usize,
    // Includes a short-lived reservation while a writer owns its segment lock.
    total_len: AtomicUsize,
    // Only cross-segment eviction takes this lock, always BEFORE any segment
    // lock. Ordinary updates/lookups and local replacement remain independent.
    pressure: Mutex<()>,
    rs: RandomState,
}

impl<K: Copy + Eq + Hash> BoundedScopedMap<K> {
    pub fn new(capacity: usize) -> Self {
        // Preserve the deployed rounding and minimum physical size exactly.
        let seg_cap = (capacity.max(1) / SEGMENTS).max(64);
        let segs = (0..SEGMENTS)
            .map(|_| {
                Mutex::new(Segment {
                    storage: None,
                    len: 0,
                })
            })
            .collect();
        Self {
            segs,
            seg_cap,
            total_len: AtomicUsize::new(0),
            pressure: Mutex::new(()),
            rs: RandomState::new(),
        }
    }

    #[inline]
    fn hash(&self, key: &K) -> u64 {
        self.rs.hash_one(key)
    }

    #[inline]
    fn seg_of(&self, hash: u64) -> usize {
        (hash >> 26) as usize % SEGMENTS
    }

    pub fn len(&self) -> usize {
        self.total_len.load(Ordering::Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, key: &K) -> Option<i64> {
        let hash = self.hash(key);
        let seg = self.segs[self.seg_of(hash)].lock();
        if seg.len == 0 {
            return None;
        }
        let home = hash as usize % self.seg_cap;
        for probe in 0..PROBES {
            if let Some((stored, expiry)) = seg.slot((home + probe) % self.seg_cap)
                && stored == *key
            {
                return Some(expiry);
            }
        }
        None
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    fn reserve(&self, soft_cap: usize) -> bool {
        self.total_len
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < soft_cap.max(1)).then(|| count + 1)
            })
            .is_ok()
    }

    /// Existing rows survive a budget reduction; subsequent insertions replace
    /// rows until expiry/removal reduces occupancy. New slots are atomically
    /// reserved so simultaneous writers cannot overshoot the current budget.
    pub fn insert(&self, key: K, expiry: i64, now: i64, soft_cap: usize) -> Option<K> {
        if now >= expiry {
            self.remove(&key);
            return None;
        }
        let hash = self.hash(&key);
        let seg_index = self.seg_of(hash);
        let home = hash as usize % self.seg_cap;
        let mut pressure = None;
        loop {
            let mut seg = self.segs[seg_index].lock();
            let mut dead = None;
            let mut victim = None;
            let mut victim_expiry = i64::MAX;
            for probe in 0..PROBES {
                let index = (home + probe) % self.seg_cap;
                match seg.slot(index) {
                    None => {
                        if dead.is_none() {
                            dead = Some(index);
                        }
                    }
                    Some((stored, value)) => {
                        if stored == key {
                            seg.put(index, Some((key, expiry)), self.seg_cap);
                            return None;
                        }
                        if victim.is_none() || value < victim_expiry {
                            victim = Some(index);
                            victim_expiry = value;
                        }
                    }
                }
            }
            if let Some(index) = dead
                && self.reserve(soft_cap)
            {
                seg.put(index, Some((key, expiry)), self.seg_cap);
                seg.len += 1;
                return None;
            }
            if let Some(index) = victim {
                let (evicted, _) = seg.slot(index).expect("live victim");
                seg.put(index, Some((key, expiry)), self.seg_cap);
                return Some(evicted);
            }
            let index = dead.expect("a window without victims has a free slot");
            if pressure.is_none() {
                // Never acquire the cross-segment lock with a shard held.
                drop(seg);
                pressure = Some(self.pressure.lock());
                continue;
            }
            // Transfer the victim's global reservation to the incoming row;
            // do not briefly decrement the counter and expose a stolen slot.
            if let Some(evicted) = self.evict_elsewhere(seg_index) {
                seg.put(index, Some((key, expiry)), self.seg_cap);
                seg.len += 1;
                return Some(evicted);
            }
            // All other segments can be empty while this segment has rows
            // outside the incoming key's probe window.
            if let Some(victim) = seg.earliest() {
                let (evicted, _) = seg.slot(victim).expect("live local victim");
                seg.put(victim, None, self.seg_cap);
                seg.put(index, Some((key, expiry)), self.seg_cap);
                return Some(evicted);
            }
            // Concurrent removals may have emptied the table during the scan.
            // Re-probe and reserve; never insert an unaccounted row.
            drop(seg);
        }
    }

    // Called only with pressure and the destination segment held. Other
    // cross-segment writers acquire pressure before their destination locks.
    fn evict_elsewhere(&self, skip: usize) -> Option<K> {
        for offset in 1..SEGMENTS {
            let mut other = self.segs[(skip + offset) % SEGMENTS].lock();
            if other.len == 0 {
                continue;
            }
            if let Some(index) = other.earliest() {
                let (key, _) = other.slot(index).expect("live victim");
                other.put(index, None, self.seg_cap);
                other.len -= 1;
                return Some(key);
            }
        }
        None
    }

    #[cfg(test)]
    pub fn seed_unchecked(&self, key: K, expiry: i64) {
        let hash = self.hash(&key);
        let mut seg = self.segs[self.seg_of(hash)].lock();
        let home = hash as usize % self.seg_cap;
        for probe in 0..PROBES {
            let index = (home + probe) % self.seg_cap;
            if seg.slot(index).is_none() {
                seg.put(index, Some((key, expiry)), self.seg_cap);
                seg.len += 1;
                self.total_len.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        panic!("test seed found no free slot in probe window");
    }

    pub fn remove(&self, key: &K) -> bool {
        self.remove_matching(key, |_| true)
    }
    pub fn remove_if(&self, key: &K, expected: i64) -> bool {
        self.remove_matching(key, |expiry| expiry == expected)
    }
    fn remove_matching(&self, key: &K, matches: impl Fn(i64) -> bool) -> bool {
        let hash = self.hash(key);
        let mut seg = self.segs[self.seg_of(hash)].lock();
        if seg.len == 0 {
            return false;
        }
        let home = hash as usize % self.seg_cap;
        for probe in 0..PROBES {
            let index = (home + probe) % self.seg_cap;
            if let Some((stored, expiry)) = seg.slot(index)
                && stored == *key
                && matches(expiry)
            {
                seg.put(index, None, self.seg_cap);
                seg.len -= 1;
                self.total_len.fetch_sub(1, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    pub fn for_each(&self, mut visit: impl FnMut(K, i64) -> bool) {
        for seg in &self.segs {
            let seg = seg.lock();
            if seg.len == 0 {
                continue;
            }
            if let Some(storage) = &seg.storage {
                for (_, key, expiry) in storage.entries() {
                    if !visit(key, expiry) {
                        return;
                    }
                }
            }
        }
    }

    /// Scan occupancy words and live values, without touching empty payload
    /// pages. A completely empty segment releases both reservations and bits.
    pub fn retain(&self, mut keep: impl FnMut(i64) -> bool) {
        for seg in &self.segs {
            let mut seg = seg.lock();
            let mut removed = 0;
            if let Some(storage) = &mut seg.storage {
                for (word, mask) in storage.occupied.iter_mut().enumerate() {
                    let mut remaining = *mask;
                    while remaining != 0 {
                        let bit = remaining.trailing_zeros() as usize;
                        remaining &= remaining - 1;
                        let index = word * 64 + bit;
                        // SAFETY: as in entries(), every set bit denotes a
                        // fully initialized Copy value under the shard lock.
                        let (_, expiry) = unsafe { storage.slots[index].assume_init() };
                        if !keep(expiry) {
                            *mask &= !(1 << bit);
                            removed += 1;
                        }
                    }
                }
            }
            seg.len -= removed;
            if removed > 0 {
                self.total_len.fetch_sub(removed, Ordering::Relaxed);
            }
            if seg.len == 0 {
                seg.storage = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tables_reserve_no_payload_and_sweep_releases_empty_segments() {
        let map = BoundedScopedMap::<u64>::new(8_000_000);
        assert!(map.segs.iter().all(|seg| seg.lock().storage.is_none()));
        map.insert(1, 100, 0, 8_000_000);
        assert_eq!(
            map.segs
                .iter()
                .filter(|seg| seg.lock().storage.is_some())
                .count(),
            1
        );
        assert_eq!(map.get(&1), Some(100));
        map.retain(|_| false);
        assert!(map.is_empty());
        assert!(map.segs.iter().all(|seg| seg.lock().storage.is_none()));
        map.insert(1, 200, 0, 8_000_000);
        assert_eq!(map.get(&1), Some(200));
    }

    #[test]
    fn occupancy_bits_guard_uninitialized_slots_including_partial_last_word() {
        let capacity = 131;
        let mut storage = SlotStorage::<usize>::new(capacity);
        for index in 0..capacity {
            assert_eq!(storage.get(index), None);
        }
        for index in [0, 63, 64, 127, 128, 130] {
            storage.put(index, Some((index, 100)));
        }
        for index in 0..capacity {
            assert_eq!(
                storage.get(index),
                [0, 63, 64, 127, 128, 130]
                    .contains(&index)
                    .then_some((index, 100))
            );
        }
        assert_eq!(
            storage
                .entries()
                .map(|(index, _, _)| index)
                .collect::<Vec<_>>(),
            [0, 63, 64, 127, 128, 130]
        );
        storage.put(64, None);
        assert_eq!(storage.get(64), None);
        storage.put(64, Some((999, 200)));
        assert_eq!(storage.get(64), Some((999, 200)));
    }

    #[test]
    fn concurrent_small_budget_progresses_and_never_overshoots() {
        for capacity in [1, 7, 64] {
            let map = std::sync::Arc::new(BoundedScopedMap::new(65_536));
            let (tx, rx) = std::sync::mpsc::channel();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let mut handles = Vec::new();
            for worker in 0..8u64 {
                let map = map.clone();
                let tx = tx.clone();
                let barrier = barrier.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    for index in 0..2000 {
                        map.insert(worker * 2000 + index, i64::MAX, 0, capacity);
                        assert!(map.len() <= capacity, "soft cap overshoot");
                    }
                    tx.send(()).unwrap();
                }));
            }
            drop(tx);
            for _ in 0..8 {
                rx.recv_timeout(std::time::Duration::from_secs(10))
                    .expect("cross-segment eviction must make progress");
            }
            for handle in handles {
                handle.join().unwrap();
            }
            let mut visited = 0;
            map.for_each(|_, _| {
                visited += 1;
                true
            });
            assert_eq!(map.len(), visited);
            assert_eq!(visited, capacity);
        }
    }

    #[test]
    fn returned_victims_keep_a_reference_model_consistent_under_churn() {
        let mut map = BoundedScopedMap::new(4096);
        map.rs = RandomState::with_seeds(11, 22, 33, 44);
        let mut expected = std::collections::HashMap::new();
        for step in 0..5000u64 {
            let key = step.wrapping_mul(2654435761) % 211;
            if step % 7 == 0 {
                assert_eq!(map.remove(&key), expected.remove(&key).is_some());
            } else {
                let expiry = 100 + step as i64;
                if let Some(victim) = map.insert(key, expiry, 0, 31) {
                    assert!(expected.remove(&victim).is_some());
                }
                expected.insert(key, expiry);
            }
            assert_eq!(map.len(), expected.len());
            assert!(map.len() <= 31);
            for (key, expiry) in &expected {
                assert_eq!(map.get(key), Some(*expiry));
            }
        }
    }

    #[test]
    fn insert_get_remove_roundtrip() {
        let map = BoundedScopedMap::new(1024);
        assert_eq!(map.insert(7u64, 100, 10, 1024), None);
        assert_eq!(map.get(&7), Some(100));
        assert_eq!(map.len(), 1);
        assert_eq!(map.insert(7, 200, 10, 1024), None);
        assert_eq!(map.get(&7), Some(200));
        assert_eq!(map.len(), 1);
        assert!(map.remove(&7));
        assert_eq!(map.len(), 0);
        assert!(!map.remove(&7));
    }

    #[test]
    fn physical_cap_is_enforced_by_eviction() {
        let map = BoundedScopedMap::new(64 * 64);
        for i in 0..(64 * 64 * 2) {
            map.insert(i as u64, i as i64 + 1000, 10, 64 * 64);
        }
        assert!(
            map.len() <= 64 * 64,
            "physical bound must hold under flood: {}",
            map.len()
        );
    }

    #[test]
    fn soft_cap_evicts_earliest_expiry() {
        let map = BoundedScopedMap::new(4096);
        for i in 0..100u64 {
            map.insert(i, 500, 10, 100);
        }
        assert_eq!(map.len(), 100);
        // Next insert must evict someone even though dead slots exist.
        let evicted = map.insert(999, 600, 10, 100);
        assert!(evicted.is_some());
        assert_eq!(map.len(), 100);
        assert_eq!(map.get(&999), Some(600));
    }

    #[test]
    fn expired_insert_is_a_remove() {
        let map = BoundedScopedMap::new(1024);
        map.insert(5u64, 100, 10, 1024);
        assert_eq!(map.insert(5, 5, 10, 1024), None);
        assert_eq!(map.get(&5), None);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn retain_sweeps_dead_entries() {
        let map = BoundedScopedMap::new(1024);
        for i in 0..50u64 {
            map.insert(i, i as i64, 0, 1024);
        }
        map.retain(|expiry| expiry > 25);
        assert_eq!(map.len(), 24);
        assert_eq!(map.get(&10), None);
        assert_eq!(map.get(&40), Some(40));
    }

    #[test]
    fn remove_if_only_drops_matching_expiry() {
        let map = BoundedScopedMap::new(1024);
        map.insert(9u64, 100, 0, 1024);
        assert!(!map.remove_if(&9, 50));
        assert_eq!(map.get(&9), Some(100));
        assert!(map.remove_if(&9, 100));
        assert_eq!(map.get(&9), None);
    }

    #[test]
    fn concurrent_flood_never_exceeds_cap() {
        use std::sync::Arc;
        let map = Arc::new(BoundedScopedMap::new(64 * 64));
        let mut handles = Vec::new();
        for t in 0..8 {
            let m = map.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20_000u64 {
                    m.insert(
                        i.wrapping_mul(2654435761) ^ t,
                        i as i64 + 10_000,
                        5,
                        64 * 64,
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(map.len() <= 64 * 64);
    }
}
