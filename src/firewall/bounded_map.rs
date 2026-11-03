//! Fixed-address, bounded scoped-state table for attacker-driven keys.
//!
//! Hashing and eight-slot replacement windows never change as the table fills.
//! Slot storage is allocated in 64-slot chunks only on first write; empty
//! tables allocate no slot payload. Retain reclaims empty chunks. The configured
//! slot bound and earliest-expiry replacement policy remain in force.

use ahash::RandomState;
use parking_lot::Mutex;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};

const PROBES: usize = 8;
const SEGMENTS: usize = 64;
const CHUNK_SLOTS: usize = 64;

type Slot<K> = Option<(K, i64)>;

struct Segment<K> {
    chunks: Vec<Option<Box<[Slot<K>]>>>,
    len: usize,
}

impl<K: Copy> Segment<K> {
    fn slot(&self, index: usize) -> Slot<K> {
        self.chunks
            .get(index / CHUNK_SLOTS)
            .and_then(Option::as_ref)
            .and_then(|chunk| chunk[index % CHUNK_SLOTS])
    }

    fn put(&mut self, index: usize, value: Slot<K>, capacity: usize) {
        let chunk_index = index / CHUNK_SLOTS;
        if self.chunks.is_empty() {
            if value.is_none() {
                return;
            }
            self.chunks
                .resize_with(capacity.div_ceil(CHUNK_SLOTS), || None);
        }
        if self.chunks[chunk_index].is_none() {
            if value.is_none() {
                return;
            }
            let length = CHUNK_SLOTS.min(capacity - chunk_index * CHUNK_SLOTS);
            self.chunks[chunk_index] = Some(vec![None; length].into_boxed_slice());
        }
        self.chunks[chunk_index].as_mut().expect("allocated chunk")[index % CHUNK_SLOTS] = value;
    }

    fn earliest(&self) -> Option<usize> {
        let mut best = None;
        let mut expiry = i64::MAX;
        for (chunk_index, chunk) in self.chunks.iter().enumerate() {
            if let Some(chunk) = chunk {
                for (offset, slot) in chunk.iter().enumerate() {
                    if let Some((_, value)) = slot
                        && (best.is_none() || *value < expiry)
                    {
                        best = Some(chunk_index * CHUNK_SLOTS + offset);
                        expiry = *value;
                    }
                }
            }
        }
        best
    }
}

/// Bounded map with immutable slot addresses. Lookups and ordinary inserts
/// probe eight slots. Cold iteration scans allocated chunks, not reserved space.
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
                    chunks: Vec::new(),
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
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
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
            for chunk in seg.chunks.iter().flatten() {
                for &(key, expiry) in chunk.iter().flatten() {
                    if !visit(key, expiry) {
                        return;
                    }
                }
            }
        }
    }

    /// Full semantic sweep over allocated chunks. Empty payload is returned to
    /// the allocator; unchanged hash addresses keep remaining keys reachable.
    pub fn retain(&self, mut keep: impl FnMut(i64) -> bool) {
        for seg in &self.segs {
            let mut seg = seg.lock();
            let mut removed = 0;
            for entry in &mut seg.chunks {
                if let Some(chunk) = entry {
                    let mut live = 0;
                    for slot in chunk.iter_mut() {
                        if let Some((_, expiry)) = *slot {
                            if keep(expiry) {
                                live += 1;
                            } else {
                                *slot = None;
                                removed += 1;
                            }
                        }
                    }
                    if live == 0 {
                        *entry = None;
                    }
                }
            }
            seg.len -= removed;
            if removed > 0 {
                self.total_len.fetch_sub(removed, Ordering::Relaxed);
            }
            if seg.len == 0 {
                seg.chunks = Vec::new();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tables_allocate_no_slot_payload_and_sweeps_release_empty_chunks() {
        let map = BoundedScopedMap::<u64>::new(8_000_000);
        assert!(map.segs.iter().all(|seg| seg.lock().chunks.is_empty()));
        map.insert(1, 100, 0, 8_000_000);
        let allocated = map
            .segs
            .iter()
            .map(|seg| {
                seg.lock()
                    .chunks
                    .iter()
                    .flatten()
                    .map(|chunk| chunk.len())
                    .sum::<usize>()
            })
            .sum::<usize>();
        assert_eq!(allocated, CHUNK_SLOTS);
        assert_eq!(map.get(&1), Some(100));
        map.retain(|_| false);
        assert!(map.is_empty());
        assert!(map.segs.iter().all(|seg| seg.lock().chunks.is_empty()));
        map.insert(1, 200, 0, 8_000_000);
        assert_eq!(map.get(&1), Some(200));
    }

    #[test]
    fn chunk_edges_and_partial_last_chunk_keep_fixed_slot_addresses() {
        let mut segment = Segment {
            chunks: Vec::new(),
            len: 0,
        };
        let capacity = CHUNK_SLOTS * 2 + 3;
        for index in [0, 63, 64, 127, 128, 130] {
            segment.put(index, Some((index, 100)), capacity);
        }
        for index in 0..capacity {
            assert_eq!(
                segment.slot(index),
                [0, 63, 64, 127, 128, 130]
                    .contains(&index)
                    .then_some((index, 100))
            );
        }
        assert_eq!(segment.chunks[2].as_ref().unwrap().len(), 3);
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
