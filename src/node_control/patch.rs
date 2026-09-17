//! Ordered resource patch journal. Does not mutate the live ConfigStore.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchOp {
    pub release_id: String,
    pub seq: i64,
    pub digest: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchApplyError {
    OutOfOrder { expected: i64, actual: i64 },
    Gap { expected: i64, actual: i64 },
    ApplyFailed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchResult {
    pub idempotent_replay: bool,
    pub snapshot_required: bool,
}

#[derive(Debug, Default)]
pub struct PatchJournal {
    last_seq: i64,
    last_digest: Vec<u8>,
    last_release_id: String,
    snapshot_required: bool,
    /// Opaque last-success payload. Unchanged on failed apply.
    active_payload: Vec<u8>,
}

impl PatchJournal {
    pub fn last_seq(&self) -> i64 {
        self.last_seq
    }

    pub fn snapshot_required(&self) -> bool {
        self.snapshot_required || self.last_seq == 0
    }

    pub fn active_payload(&self) -> &[u8] {
        &self.active_payload
    }

    pub fn last_release_id(&self) -> &str {
        &self.last_release_id
    }

    pub fn last_digest(&self) -> &[u8] {
        &self.last_digest
    }

    /// First sync / recovery: load a signed snapshot without requiring seq=1.
    pub fn install_snapshot(
        &mut self,
        release_id: String,
        seq: i64,
        digest: Vec<u8>,
        payload: Vec<u8>,
    ) {
        self.last_release_id = release_id;
        self.last_seq = seq;
        self.last_digest = digest;
        self.active_payload = payload;
        self.snapshot_required = false;
    }

    /// Sequence check without committing. Gap marks `snapshot_required` because
    /// callers must fall back to a full snapshot; active payload is unchanged.
    pub fn inspect(&mut self, op: &PatchOp) -> Result<PatchResult, PatchApplyError> {
        if op.seq == self.last_seq && op.digest == self.last_digest {
            return Ok(PatchResult {
                idempotent_replay: true,
                snapshot_required: self.snapshot_required,
            });
        }
        if self.last_seq == 0 {
            self.snapshot_required = true;
            return Err(PatchApplyError::Gap {
                expected: 1,
                actual: op.seq,
            });
        }
        let expected = self.last_seq + 1;
        if op.seq < expected {
            return Err(PatchApplyError::OutOfOrder {
                expected,
                actual: op.seq,
            });
        }
        if op.seq > expected {
            self.snapshot_required = true;
            return Err(PatchApplyError::Gap {
                expected,
                actual: op.seq,
            });
        }
        if op.payload == b"FAIL" {
            return Err(PatchApplyError::ApplyFailed {
                reason: "injected apply failure".into(),
            });
        }
        Ok(PatchResult {
            idempotent_replay: false,
            snapshot_required: false,
        })
    }

    pub fn apply(&mut self, op: &PatchOp) -> Result<PatchResult, PatchApplyError> {
        let preview = self.inspect(op)?;
        if preview.idempotent_replay {
            return Ok(preview);
        }
        self.last_seq = op.seq;
        self.last_digest = op.digest.clone();
        self.last_release_id = op.release_id.clone();
        self.active_payload = op.payload.clone();
        self.snapshot_required = false;
        Ok(PatchResult {
            idempotent_replay: false,
            snapshot_required: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(seq: i64, payload: &[u8]) -> PatchOp {
        PatchOp {
            release_id: format!("rel-{seq}"),
            seq,
            digest: payload.to_vec(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn duplicate_seq_is_idempotent() {
        let mut j = PatchJournal::default();
        j.install_snapshot("rel-0".into(), 3, b"base".to_vec(), b"active".to_vec());
        let first = j.apply(&op(4, b"p4")).unwrap();
        assert!(!first.idempotent_replay);
        let replay = j.apply(&op(4, b"p4")).unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(j.active_payload(), b"p4");
    }

    #[test]
    fn out_of_order_is_rejected() {
        let mut j = PatchJournal::default();
        j.install_snapshot("rel-0".into(), 2, b"b".to_vec(), b"a".to_vec());
        let err = j.apply(&op(2, b"other")).unwrap_err();
        assert!(matches!(err, PatchApplyError::OutOfOrder { .. }));
        assert_eq!(j.active_payload(), b"a");
    }

    #[test]
    fn gap_requests_snapshot_and_keeps_active() {
        let mut j = PatchJournal::default();
        j.install_snapshot("rel-0".into(), 1, b"b".to_vec(), b"keep".to_vec());
        let err = j.apply(&op(5, b"skip")).unwrap_err();
        assert!(matches!(
            err,
            PatchApplyError::Gap {
                expected: 2,
                actual: 5
            }
        ));
        assert!(j.snapshot_required());
        assert_eq!(j.active_payload(), b"keep");
    }

    #[test]
    fn failed_apply_does_not_change_active() {
        let mut j = PatchJournal::default();
        j.install_snapshot("rel-0".into(), 1, b"b".to_vec(), b"live".to_vec());
        let err = j.apply(&op(2, b"FAIL")).unwrap_err();
        assert!(matches!(err, PatchApplyError::ApplyFailed { .. }));
        assert_eq!(j.last_seq(), 1);
        assert_eq!(j.active_payload(), b"live");
    }

    #[test]
    fn first_patch_without_snapshot_requests_snapshot() {
        let mut j = PatchJournal::default();
        assert!(j.snapshot_required());
        let err = j.apply(&op(1, b"x")).unwrap_err();
        assert!(matches!(err, PatchApplyError::Gap { .. }));
        assert!(j.active_payload().is_empty());
    }
}
