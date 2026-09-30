//! GetDesiredRelease view over the local journal and live config version cursor.

use crate::node_control::patch::PatchJournal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRelease {
    pub release_id: String,
    pub signature_digest: Vec<u8>,
    pub snapshot_required: bool,
    /// Migration window: control plane may still send full `nodeJSON`.
    pub compatibility_full_snapshot: bool,
    pub local_config_version: i64,
}

pub fn desired_release(journal: &PatchJournal) -> DesiredRelease {
    desired_release_for_cursor(journal, 0)
}

pub fn desired_release_for_cursor(journal: &PatchJournal, local_config_version: i64) -> DesiredRelease {
    let snapshot_required = journal.snapshot_required() || local_config_version == 0;
    DesiredRelease {
        release_id: journal.last_release_id().to_string(),
        signature_digest: journal.last_digest().to_vec(),
        snapshot_required,
        compatibility_full_snapshot: true,
        local_config_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_control::patch::PatchJournal;

    #[test]
    fn empty_journal_requires_snapshot() {
        let journal = PatchJournal::default();
        let desired = desired_release(&journal);
        assert!(desired.snapshot_required);
        assert!(desired.compatibility_full_snapshot);
        assert!(desired.release_id.is_empty());
    }

    #[test]
    fn after_snapshot_patches_do_not_require_snapshot() {
        let mut journal = PatchJournal::default();
        journal.install_snapshot("rel-9".into(), 9, b"sig".to_vec(), b"cfg".to_vec());
        let desired = desired_release_for_cursor(&journal, 9);
        assert!(!desired.snapshot_required);
        assert!(desired.compatibility_full_snapshot);
        assert_eq!(desired.release_id, "rel-9");
        assert_eq!(desired.signature_digest, b"sig");
        assert_eq!(desired.local_config_version, 9);
    }

    #[test]
    fn gap_marks_snapshot_required_against_live_cursor() {
        let mut journal = PatchJournal::default();
        journal.install_snapshot("rel-1".into(), 1, b"a".to_vec(), b"keep".to_vec());
        let _ = journal.apply(&crate::node_control::patch::PatchOp {
            release_id: "rel-9".into(),
            seq: 9,
            digest: b"gap".to_vec(),
            payload: b"skip".to_vec(),
        });
        let desired = desired_release_for_cursor(&journal, 1);
        assert!(desired.snapshot_required);
        assert!(desired.compatibility_full_snapshot);
        assert_eq!(desired.local_config_version, 1);
    }
}
