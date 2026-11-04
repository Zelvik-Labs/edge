// Included with std or Loom AtomicU64/Ordering/fence in the enclosing module.
// Writers must hold the existing snapshot writer mutex for this entire protocol.
pub(crate) struct SnapshotSequence {
    version: AtomicU64,
}

impl SnapshotSequence {
    pub(crate) fn new() -> Self {
        Self {
            version: AtomicU64::new(0),
        }
    }

    #[inline]
    pub(crate) fn begin_read(&self) -> Option<u64> {
        let version = self.version.load(Ordering::Acquire);
        (version & 1 == 0).then_some(version)
    }

    #[inline]
    pub(crate) fn read_valid(&self, version: u64) -> bool {
        // Pairs with the writer's release fence if any relaxed field read
        // observes that publication. The following version load then cannot
        // still accept the version preceding that writer's odd marker.
        fence(Ordering::Acquire);
        self.version.load(Ordering::Relaxed) == version
    }

    pub(crate) fn begin_write(&self) -> u64 {
        // Also recovers an odd marker left by a panicking previous writer.
        // No Drop guard may publish a partially written snapshot as complete.
        let odd = self.version.load(Ordering::Relaxed) | 1;
        self.version.store(odd, Ordering::Relaxed);
        fence(Ordering::Release);
        odd.wrapping_add(1)
    }

    pub(crate) fn finish_write(&self, version: u64) {
        self.version.store(version, Ordering::Release);
    }
}
