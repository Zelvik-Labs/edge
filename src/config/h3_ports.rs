//! Immutable site-listener membership, published with the configuration snapshot.
use std::sync::Arc;

#[derive(Clone, Default)]
pub(super) struct Http3SitePorts(Option<Arc<[u64; 1024]>>);

impl Http3SitePorts {
    pub(super) fn contains(&self, port: u16) -> bool {
        self.0
            .as_ref()
            .is_some_and(|words| words[usize::from(port) / 64] & (1 << (port % 64)) != 0)
    }

    pub(super) fn insert_range(&mut self, start: u16, end: u16) {
        let words = Arc::make_mut(self.0.get_or_insert_with(|| Arc::new([0; 1024])));
        let first = usize::from(start) / 64;
        let last = usize::from(end) / 64;
        let left = u64::MAX << (start % 64);
        let right = u64::MAX >> (63 - end % 64);
        if first == last {
            words[first] |= left & right;
        } else {
            words[first] |= left;
            words[first + 1..last].fill(u64::MAX);
            words[last] |= right;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_port_matches_range_union_at_word_boundaries() {
        for ranges in [
            vec![],
            vec![(0, 0)],
            vec![(65535, 65535)],
            vec![(0, 65535)],
            vec![(63, 64), (127, 128), (65534, 65535)],
            vec![(0, 63), (64, 127), (443, 444), (440, 445)],
            vec![(1, 1), (62, 62), (65, 126), (129, 65534)],
        ] {
            let mut index = Http3SitePorts::default();
            for &(start, end) in &ranges {
                index.insert_range(start, end);
            }
            for port in 0..=u16::MAX {
                assert_eq!(
                    index.contains(port),
                    ranges.iter().any(|&(a, b)| (a..=b).contains(&port)),
                    "port={port}, ranges={ranges:?}"
                );
            }
        }
    }

    #[test]
    fn snapshot_clone_does_not_change_when_a_new_index_is_extended() {
        let mut index = Http3SitePorts::default();
        assert!(index.0.is_none());
        index.insert_range(443, 444);
        let old = index.clone();
        assert!(Arc::ptr_eq(
            index.0.as_ref().unwrap(),
            old.0.as_ref().unwrap()
        ));
        index.insert_range(8443, 8443);
        assert!(!old.contains(8443));
        assert!(index.contains(8443));
        assert!(old.contains(443) && index.contains(443));
    }
}
