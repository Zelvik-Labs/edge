//! CloudSidra `NodeControl` v1 skeleton.
//!
//! Isolated from the HTTP/cache/WAF/XDP data plane. Legacy
//! `pb.NodeService.find_current_node_config` remains the migration-window
//! snapshot path. `index.IndexService.register_node` is never used as an alias.

#[cfg(feature = "node-control")]
pub mod proto {
    tonic::include_proto!("cloudsidra.node.v1");
}

pub mod apply;
pub mod desired;
pub mod events;
pub mod health;
pub mod patch;
pub mod register;
pub mod store;

pub use apply::{ApplyError, ApplyMachine, Phase, PhaseStatus};
pub use desired::{DesiredRelease, desired_release};
pub use events::{EventAck, EventBatchError, EventBatchIngress};
pub use health::NodeHealthReport;
pub use patch::{PatchApplyError, PatchJournal, PatchOp};
pub use register::{CommMode, RegisterError, RegisterInput, RegisterOutcome, enroll};
pub use store::{StoreApplyError, StoreApplyResult, apply_node_payload_json};

/// Fully-qualified service name. Must not be confused with `index.IndexService`.
pub const NODE_CONTROL_SERVICE: &str = "cloudsidra.node.v1.NodeControl";
pub const INDEX_REGISTER_SERVICE: &str = "index.IndexService";

#[cfg(all(test, feature = "node-control"))]
mod proto_tests {
    use super::apply::Phase;
    use super::proto::{
        ApplyReleaseRequest, EnvelopeMeta, GetDesiredReleaseResponse, RegisterNodeRequest,
        ReportNodeHealthRequest, StreamEventsRequest,
    };

    fn meta() -> EnvelopeMeta {
        EnvelopeMeta {
            schema_version: "node.v1".into(),
            base_version: 1,
            patch_seq: 2,
            min_reader_version: "node.v1".into(),
        }
    }

    #[test]
    fn generated_register_message_lives_in_node_control_package() {
        let name = std::any::type_name::<RegisterNodeRequest>();
        assert!(name.contains("cloudsidra"), "{name}");
    }

    #[test]
    fn apply_release_wire_phase_field_is_string() {
        let req = ApplyReleaseRequest {
            meta: Some(meta()),
            node_id: "n1".into(),
            release_id: "rel-1".into(),
            phase: "prepare".into(),
        };
        assert_eq!(Phase::parse(&req.phase).unwrap(), Phase::Prepare);
        assert!(Phase::parse("switch").is_err());
    }

    #[test]
    fn get_desired_release_snapshot_flag() {
        let resp = GetDesiredReleaseResponse {
            meta: Some(meta()),
            release_id: String::new(),
            signature_digest: vec![],
            snapshot_required: true,
        };
        assert!(resp.snapshot_required);
    }

    #[test]
    fn health_and_event_messages_carry_structured_fields() {
        let health = ReportNodeHealthRequest {
            meta: Some(meta()),
            node_id: "n1".into(),
            ready: true,
            listeners_json: "[]".into(),
            cache_json: "{}".into(),
            xdp_json: "{}".into(),
            resource_pressure_json: "{}".into(),
            task_status_json: "{}".into(),
        };
        assert!(health.ready);
        let ev = StreamEventsRequest {
            meta: Some(meta()),
            node_id: "n1".into(),
            sequence: 7,
            payload: b"batch".to_vec(),
        };
        assert_eq!(ev.sequence, 7);
    }
}

#[cfg(test)]
mod alias_guard {
    use super::*;

    #[test]
    fn register_node_is_not_index_service_alias() {
        assert_ne!(NODE_CONTROL_SERVICE, INDEX_REGISTER_SERVICE);
        assert!(
            !NODE_CONTROL_SERVICE.contains("IndexService"),
            "NodeControl must not share IndexService identity"
        );
    }
}
