//! CPU identities inherited by the bridge supervisor, not a CPU-count range.
use std::io;

// Accommodate kernels configured with more CPUs than libc::cpu_set_t. Bound
// startup allocation even if an unexpected kernel keeps returning EINVAL.
const MAX_AFFINITY_BYTES: usize = 1024 * 1024;

pub(super) struct CpuAffinity {
    ids: Vec<u32>,
    mask_words: usize,
}

impl CpuAffinity {
    pub(super) fn current() -> io::Result<Self> {
        let mut mask =
            vec![0usize; std::mem::size_of::<libc::cpu_set_t>() / std::mem::size_of::<usize>()];
        loop {
            let bytes = std::mem::size_of_val(mask.as_slice());
            // SAFETY: mask is aligned for cpu_set_t and has exactly bytes
            // writable bytes. Linux accepts larger dynamically sized masks.
            let result = unsafe { libc::sched_getaffinity(0, bytes, mask.as_mut_ptr().cast()) };
            if result == 0 {
                return Self::from_mask(&mask);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) || bytes >= MAX_AFFINITY_BYTES {
                return Err(error);
            }
            mask.resize(mask.len() * 2, 0);
        }
    }

    fn from_mask(mask: &[usize]) -> io::Result<Self> {
        let mut ids = Vec::new();
        for (word, bits) in mask.iter().enumerate() {
            let mut remaining = *bits;
            while remaining != 0 {
                let bit = remaining.trailing_zeros() as usize;
                ids.push((word * usize::BITS as usize + bit) as u32);
                remaining &= remaining - 1;
            }
        }
        if ids.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CPU affinity is empty",
            ));
        }
        Ok(Self {
            ids,
            mask_words: mask.len(),
        })
    }

    pub(super) fn automatic_cpu(&self, ordinal: usize) -> u32 {
        self.ids[ordinal % self.ids.len()]
    }

    pub(super) fn pin_current_thread(&self, cpu: u32) -> io::Result<()> {
        let cpu = cpu as usize;
        if cpu >= self.mask_words * usize::BITS as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "configured CPU exceeds the kernel affinity mask",
            ));
        }
        let mut mask = vec![0usize; self.mask_words];
        mask[cpu / usize::BITS as usize] |= 1usize << (cpu % usize::BITS as usize);
        // SAFETY: the initialized mask has the same size accepted by the
        // kernel query. pid 0 changes only this worker, and one bit is set.
        if unsafe {
            libc::sched_setaffinity(
                0,
                std::mem::size_of_val(mask.as_slice()),
                mask.as_ptr().cast(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_assignment_uses_sparse_cpu_ids_not_their_count() {
        let affinity = CpuAffinity::from_mask(&[(1 << 3) | (1 << 7)]).unwrap();
        assert_eq!(
            (0..5)
                .map(|n| affinity.automatic_cpu(n))
                .collect::<Vec<_>>(),
            [3, 7, 3, 7, 3]
        );
        assert!(CpuAffinity::from_mask(&[0]).is_err());
        let wide = CpuAffinity::from_mask(&[1 << 3, 1 << 5]).unwrap();
        assert_eq!(wide.automatic_cpu(1), usize::BITS + 5);
        assert_eq!(wide.automatic_cpu(2), 3);
        assert_eq!(
            affinity.pin_current_thread(u32::MAX).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn worker_pin_stays_within_the_actual_inherited_mask() {
        let affinity = CpuAffinity::current().unwrap();
        let selected = affinity.automatic_cpu(affinity.ids.len() - 1);
        let original = affinity.ids.clone();
        std::thread::spawn(move || {
            affinity.pin_current_thread(selected).unwrap();
            assert_eq!(CpuAffinity::current().unwrap().ids, [selected]);
        })
        .join()
        .unwrap();
        assert_eq!(CpuAffinity::current().unwrap().ids, original);
    }
}
