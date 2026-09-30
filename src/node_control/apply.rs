//! ApplyRelease phase machine. Wire phases: prepare | validate | apply | ack.
//!
//! Success path is wired to the production config-apply / hot-replace entry:
//! `ApplyMachine::step_with_store(Phase::Apply, op, store)` calls
//! [`crate::node_control::store::apply_node_payload_json`], which decodes the
//! payload and drives [`crate::config::ConfigStore::update_config`]. That is the
//! same `ConfigStore::update_config` invoked by the legacy RPC config syncer
//! `crate::rpc::node::fetch_and_apply_config` (src/rpc/node.rs), so a successful
//! Apply here swaps the active config snapshot. Note: `src/rpc/node.rs` has no
//! standalone `apply_node_payload` / `update_config` function of its own; the
//! RPC path calls `update_config` inline. Kernel XDP / WAF attach side effects
//! remain on the legacy RPC path only. Wired — not a skeleton.

use crate::config::ConfigStore;
use crate::node_control::patch::{PatchApplyError, PatchJournal, PatchOp};
use crate::node_control::store::{StoreApplyError, apply_node_payload_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Prepare,
    Validate,
    Apply,
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    UnknownPhase(String),
    InternalNameForbidden(String),
    WrongOrder { have: Option<Phase>, want: Phase },
    Patch(PatchApplyError),
    Store(StoreApplyError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseStatus {
    pub phase: Phase,
    pub status: &'static str,
    pub idempotent_replay: bool,
}

impl Phase {
    pub fn parse(raw: &str) -> Result<Self, ApplyError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "prepare" => Ok(Self::Prepare),
            "validate" => Ok(Self::Validate),
            "apply" => Ok(Self::Apply),
            "ack" => Ok(Self::Ack),
            name @ ("switch" | "report") => {
                Err(ApplyError::InternalNameForbidden(name.to_string()))
            }
            other => Err(ApplyError::UnknownPhase(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Validate => "validate",
            Self::Apply => "apply",
            Self::Ack => "ack",
        }
    }
}

#[derive(Debug)]
pub struct ApplyMachine {
    journal: PatchJournal,
    prepared: Option<PatchOp>,
    validated: bool,
    last_completed: Option<Phase>,
}

impl ApplyMachine {
    pub fn new(journal: PatchJournal) -> Self {
        Self {
            journal,
            prepared: None,
            validated: false,
            last_completed: None,
        }
    }

    pub fn journal(&self) -> &PatchJournal {
        &self.journal
    }

    pub fn desired_snapshot_required(&self) -> bool {
        self.journal.snapshot_required()
    }

    /// Record a full snapshot as the journal base. Used by the NodeControl
    /// service when a node has no base yet (first sync / patch gap recovery /
    /// migration-window full snapshot). The live ConfigStore swap is the
    /// caller's responsibility and must happen before this is committed.
    /// Installing a base lets subsequent ordered patches pass the seq checks.
    pub fn install_base_snapshot(
        &mut self,
        release_id: String,
        seq: i64,
        digest: Vec<u8>,
        payload: Vec<u8>,
    ) {
        self.journal.install_snapshot(release_id, seq, digest, payload);
        self.prepared = None;
        self.validated = false;
        self.last_completed = Some(Phase::Apply);
    }

    pub fn step(&mut self, phase: Phase, op: PatchOp) -> Result<PhaseStatus, ApplyError> {
        match phase {
            Phase::Prepare => {
                if self.last_completed == Some(Phase::Prepare)
                    && self.prepared.as_ref() == Some(&op)
                {
                    return Ok(PhaseStatus {
                        phase,
                        status: "prepared",
                        idempotent_replay: true,
                    });
                }
                self.prepared = Some(op);
                self.validated = false;
                self.last_completed = Some(Phase::Prepare);
                Ok(PhaseStatus {
                    phase,
                    status: "prepared",
                    idempotent_replay: false,
                })
            }
            Phase::Validate => {
                let prepared = self.prepared.as_ref().ok_or(ApplyError::WrongOrder {
                    have: self.last_completed,
                    want: Phase::Validate,
                })?;
                if prepared.release_id != op.release_id || prepared.seq != op.seq {
                    return Err(ApplyError::WrongOrder {
                        have: self.last_completed,
                        want: Phase::Validate,
                    });
                }
                self.validated = true;
                self.last_completed = Some(Phase::Validate);
                Ok(PhaseStatus {
                    phase,
                    status: "validated",
                    idempotent_replay: false,
                })
            }
            Phase::Apply => self.apply_journal_only(op),
            Phase::Ack => {
                if self.last_completed != Some(Phase::Apply)
                    && self.last_completed != Some(Phase::Ack)
                {
                    return Err(ApplyError::WrongOrder {
                        have: self.last_completed,
                        want: Phase::Ack,
                    });
                }
                let replay = self.last_completed == Some(Phase::Ack);
                self.last_completed = Some(Phase::Ack);
                self.prepared = None;
                self.validated = false;
                Ok(PhaseStatus {
                    phase,
                    status: "acked",
                    idempotent_replay: replay,
                })
            }
        }
    }

    fn apply_journal_only(&mut self, op: PatchOp) -> Result<PhaseStatus, ApplyError> {
        if !self.validated {
            return Err(ApplyError::WrongOrder {
                have: self.last_completed,
                want: Phase::Apply,
            });
        }
        let result = self.journal.apply(&op).map_err(ApplyError::Patch)?;
        self.last_completed = Some(Phase::Apply);
        Ok(PhaseStatus {
            phase: Phase::Apply,
            status: "applied",
            idempotent_replay: result.idempotent_replay,
        })
    }

    /// Commit journal only after ConfigStore swap succeeds.
    pub async fn step_with_store(
        &mut self,
        phase: Phase,
        op: PatchOp,
        store: &ConfigStore,
    ) -> Result<PhaseStatus, ApplyError> {
        if phase != Phase::Apply {
            return self.step(phase, op);
        }
        if !self.validated {
            return Err(ApplyError::WrongOrder {
                have: self.last_completed,
                want: Phase::Apply,
            });
        }
        let preview = self.journal.inspect(&op).map_err(ApplyError::Patch)?;
        if !preview.idempotent_replay {
            apply_node_payload_json(store, &op.payload)
                .await
                .map_err(ApplyError::Store)?;
        }
        let result = self.journal.apply(&op).map_err(ApplyError::Patch)?;
        self.last_completed = Some(Phase::Apply);
        Ok(PhaseStatus {
            phase,
            status: "applied",
            idempotent_replay: result.idempotent_replay,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(seq: i64) -> PatchOp {
        PatchOp {
            release_id: "rel-1".into(),
            seq,
            digest: b"d".to_vec(),
            payload: b"ok".to_vec(),
        }
    }

    #[test]
    fn happy_path_prepare_validate_apply_ack() {
        let mut journal = PatchJournal::default();
        journal.install_snapshot("rel-0".into(), 1, b"b".to_vec(), b"old".to_vec());
        let mut m = ApplyMachine::new(journal);
        let patch = op(2);
        m.step(Phase::Prepare, patch.clone()).unwrap();
        m.step(Phase::Validate, patch.clone()).unwrap();
        m.step(Phase::Apply, patch.clone()).unwrap();
        let ack = m.step(Phase::Ack, patch).unwrap();
        assert_eq!(ack.status, "acked");
        assert_eq!(m.journal().active_payload(), b"ok");
    }

    #[test]
    fn switch_and_report_are_not_wire_phases() {
        assert!(matches!(
            Phase::parse("switch"),
            Err(ApplyError::InternalNameForbidden(_))
        ));
        assert!(matches!(
            Phase::parse("report"),
            Err(ApplyError::InternalNameForbidden(_))
        ));
    }

    #[test]
    fn apply_without_validate_is_rejected_and_keeps_active() {
        let mut journal = PatchJournal::default();
        journal.install_snapshot("rel-0".into(), 1, b"b".to_vec(), b"old".to_vec());
        let mut m = ApplyMachine::new(journal);
        let err = m.step(Phase::Apply, op(2)).unwrap_err();
        assert!(matches!(err, ApplyError::WrongOrder { .. }));
        assert_eq!(m.journal().active_payload(), b"old");
    }

    fn snapshot_json(version: i64, host: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": 1,
            "version": version,
            "isOn": true,
            "servers": [{
                "id": 9,
                "isOn": true,
                "serverNames": [{"name": host}]
            }]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn apply_with_store_swaps_config_and_keeps_old_on_parse_failure() {
        let store = crate::config::ConfigStore::new();
        let mut journal = PatchJournal::default();
        journal.install_snapshot(
            "rel-0".into(),
            1,
            b"base".to_vec(),
            snapshot_json(1, "old.example.test"),
        );
        apply_node_payload_json(&store, &snapshot_json(1, "old.example.test"))
            .await
            .unwrap();

        let mut m = ApplyMachine::new(journal);
        let good = PatchOp {
            release_id: "rel-2".into(),
            seq: 2,
            digest: b"good".to_vec(),
            payload: snapshot_json(2, "new.example.test"),
        };
        m.step(Phase::Prepare, good.clone()).unwrap();
        m.step(Phase::Validate, good.clone()).unwrap();
        m.step_with_store(Phase::Apply, good, &store)
            .await
            .unwrap();
        assert_eq!(store.get_config_version().await, 2);
        assert!(store.get_server_sync("new.example.test").is_some());

        let bad = PatchOp {
            release_id: "rel-3".into(),
            seq: 3,
            digest: b"bad".to_vec(),
            payload: b"not-json".to_vec(),
        };
        m.step(Phase::Prepare, bad.clone()).unwrap();
        m.step(Phase::Validate, bad.clone()).unwrap();
        let err = m.step_with_store(Phase::Apply, bad, &store).await.unwrap_err();
        assert!(matches!(err, ApplyError::Store(_)));
        assert_eq!(store.get_config_version().await, 2);
        assert_eq!(m.journal().last_seq(), 2);
    }
}
