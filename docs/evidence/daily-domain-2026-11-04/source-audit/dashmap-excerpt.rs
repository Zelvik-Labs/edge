    }

    fn _entry(&'a self, key: K) -> Entry<'a, K, V> {
        let hash = self.hash_u64(&key);

        let idx = self.determine_shard(hash as usize);

        let mut shard = unsafe { self._yield_write_shard(idx) };

        match shard.find_or_find_insert_slot(
            hash,
            |(k, _v)| k == &key,
            |(k, _v)| {
                let mut hasher = self.hasher.build_hasher();
                k.hash(&mut hasher);
                hasher.finish()
            },
        ) {
            Ok(elem) => Entry::Occupied(unsafe { OccupiedEntry::new(shard, key, elem) }),
            Err(slot) => Entry::Vacant(unsafe { VacantEntry::new(shard, key, hash, slot) }),
        }
    }

    fn _try_entry(&'a self, key: K) -> Option<Entry<'a, K, V>> {
        let hash = self.hash_u64(&key);
