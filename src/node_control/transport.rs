//! The migration transport shares the exact legacy snapshot apply pipeline.
//! Unsupported old APIs retain the legacy protocol; other failures never fall back.
use super::proto::{
    ApplyReleaseRequest, EnvelopeMeta, GetDesiredReleaseRequest, RegisterNodeRequest,
    ReportNodeHealthRequest,
};
use crate::api_config::ApiConfig;
use crate::pb;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tonic::Status;

pub fn verify_snapshot(
    secret: &str,
    version: i64,
    payload: &[u8],
    signature: &[u8],
) -> Result<(), Status> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| Status::internal("invalid snapshot HMAC key"))?;
    mac.update(&version.to_be_bytes());
    mac.update(payload);
    mac.verify_slice(signature)
        .map_err(|_| Status::data_loss("NodeControl snapshot signature mismatch"))
}

/// VerifiedRelease carries a release binding that survived HMAC snapshot,
/// schema, version, and Ed25519 signature checks. `version` is the frozen
/// per-node config version the API recorded at seal (sent back via
/// EnvelopeMeta.base_version on every ApplyRelease phase call).
#[derive(Debug, Clone)]
pub struct VerifiedRelease {
    pub release_id: String,
    pub version: i64,
}

/// FetchedSnapshot preserves the verified release context alongside the
/// legacy-compatible snapshot response. `release` is only populated when the
/// server offered a release binding that fully authenticated.
#[derive(Debug)]
pub struct FetchedSnapshot {
    pub config: pb::FindCurrentNodeConfigResponse,
    pub release: Option<VerifiedRelease>,
}

/// The migration transport falls back to the legacy snapshot RPC only when
/// the API predates NodeControl — every other failure stays fatal.
pub fn uses_legacy_fallback(err: &Status) -> bool {
    err.code() == tonic::Code::Unimplemented
}

pub async fn fetch_snapshot(api: &ApiConfig, version: i64) -> Result<FetchedSnapshot, Status> {
    let rpc = crate::rpc::client::SharedRpcClient::get(api)
        .await
        .map_err(|err| Status::unavailable(err.to_string()))?
        .as_rpc_client();
    let mut client = rpc.node_control_service();
    let mode = if api.rpc_tls.cert_file.is_some() {
        "mtls"
    } else if api
        .effective_rpc_endpoints()
        .iter()
        .all(|endpoint| endpoint.starts_with("https://"))
    {
        "tls"
    } else {
        "plaintext"
    };
    let registered = crate::rpc::track_rpc(client.register_node(RegisterNodeRequest {
        meta: Some(envelope(version)),
        node_id: api.node_id.clone(),
        software_version: crate::RELEASE_VERSION.into(),
        capabilities_json: r#"{"snapshotHmac":true,"structuredHealth":true}"#.into(),
        region: String::new(),
        comm_mode: mode.into(),
        cert_fingerprint: String::new(),
    }))
    .await?
    .into_inner();
    if !registered.accepted {
        return Err(Status::permission_denied(
            "NodeControl registration rejected",
        ));
    }

    let response = crate::rpc::track_rpc(client.get_desired_release(GetDesiredReleaseRequest {
        meta: Some(EnvelopeMeta {
            schema_version: "node.v1".into(),
            min_reader_version: "node.v1".into(),
            base_version: version,
            patch_seq: 0,
        }),
        node_id: api.node_id.clone(),
    }))
    .await?
    .into_inner();
    verify_snapshot_response(api, version, response)
}

/// verify_snapshot_response runs every authentication check on the offered
/// snapshot: schema, HMAC, embedded version, release signing key pin, and
/// Ed25519 release signature. Returns the legacy-compatible config view plus
/// the verified release binding.
fn verify_snapshot_response(
    api: &ApiConfig,
    version: i64,
    response: super::proto::GetDesiredReleaseResponse,
) -> Result<FetchedSnapshot, Status> {
    let meta = response
        .meta
        .ok_or_else(|| Status::data_loss("NodeControl snapshot metadata missing"))?;
    if meta.schema_version != "node.v1" || meta.min_reader_version != "node.v1" {
        return Err(Status::failed_precondition(
            "unsupported NodeControl schema",
        ));
    }
    if response.snapshot_required {
        if response.snapshot_json.is_empty() {
            return Err(Status::data_loss("NodeControl required snapshot missing"));
        }
        verify_snapshot(
            &api.secret,
            meta.base_version,
            &response.snapshot_json,
            &response.snapshot_hmac,
        )?;
        let doc: serde_json::Value = serde_json::from_slice(&response.snapshot_json)
            .map_err(|err| Status::data_loss(err.to_string()))?;
        if doc.get("version").and_then(|value| value.as_i64()) != Some(meta.base_version) {
            return Err(Status::data_loss("NodeControl snapshot version mismatch"));
        }
        // 快照已过 HMAC 认证——其中的 releaseSigning 公钥可信，先钉扎再验签。
        if let Some(signing) = doc
            .pointer("/runtime/api/releaseSigning")
            .and_then(|value| {
                serde_json::from_value::<crate::api_config::ReleaseSigningPush>(value.clone()).ok()
            })
        {
            crate::api_config::install_release_signing_key(&signing);
        }
    } else if meta.base_version != version {
        return Err(Status::data_loss(
            "NodeControl version changed without snapshot",
        ));
    } else if !response.snapshot_json.is_empty() || !response.snapshot_hmac.is_empty() {
        // Unauthenticated payload bytes must never reach the legacy apply
        // path: a valid "unchanged" response carries none.
        return Err(Status::data_loss(
            "NodeControl unchanged response carried unsigned snapshot bytes",
        ));
    }
    // Full-snapshot fetches have no patch sequencing — a nonzero cursor is a
    // malformed response, not an incremental hint.
    if meta.patch_seq != 0 {
        return Err(Status::data_loss(
            "NodeControl unexpected nonzero patch sequence",
        ));
    }
    // A nonempty release_id demands the full verification chain: the pinned
    // Ed25519 key must exist and the digest must be present and valid. The
    // legacy lenient helper stays for callers outside this transport; here a
    // release that cannot be verified is rejected, not silently carried.
    let release = if response.release_id.is_empty() {
        None
    } else {
        if meta.base_version <= 0 {
            return Err(Status::failed_precondition(
                "NodeControl release requires a positive frozen version",
            ));
        }
        if crate::api_config::release_signing_pubkey().is_none() {
            return Err(Status::failed_precondition(
                "NodeControl release offered without a pinned signing key",
            ));
        }
        if response.signature_digest.is_empty() {
            return Err(Status::data_loss(
                "NodeControl release signature_digest missing",
            ));
        }
        crate::api_config::verify_release_signature(
            &response.release_id,
            &response.signature_digest,
        )
        .map_err(|code| Status::new(code, "NodeControl release signature_digest mismatch"))?;
        Some(VerifiedRelease {
            release_id: response.release_id.clone(),
            version: meta.base_version,
        })
    };
    Ok(FetchedSnapshot {
        config: pb::FindCurrentNodeConfigResponse {
            is_changed: response.snapshot_required,
            is_compressed: false,
            data_size: response.snapshot_json.len() as i64,
            node_json: response.snapshot_json,
            timestamp: meta.base_version,
        },
        release,
    })
}

/// Fail-closed readiness check before any release phase RPC: a verified
/// release must never be confirmed while the dataplane can't carry the
/// business ports the applied config requires. Bypassed only when XDP is
/// disabled or there are no active business ports (legitimate empty).
pub(crate) fn validate_release_dataplane_snapshot(
    status: &crate::xdp::XdpStatusSnapshot,
    expected_ports: &[crate::runtime_mode::XdpProxyPortConfig],
) -> Result<(), Status> {
    if !status.enabled || expected_ports.is_empty() {
        return Ok(());
    }
    let tcp_required = expected_ports.iter().any(|p| {
        matches!(
            p.protocol,
            crate::runtime_mode::XdpProxyProtocol::Http
                | crate::runtime_mode::XdpProxyProtocol::Https
                | crate::runtime_mode::XdpProxyProtocol::Tcp
        )
    });
    let ready = status.available
        && status.attached
        && status.proxy_ports > 0
        && status.proxy_supported_ports > 0
        && status.proxy_ready
        && status.proxy_redirect_enabled
        && status.xsk_configured_queues > 0
        && status.xsk_ready_queues == status.xsk_configured_queues
        && (!tcp_required || status.tcp_dataplane_ready);
    if !ready {
        return Err(Status::failed_precondition(format!(
            "NodeControl dataplane not ready for {} business port(s): attached={} proxy_ports={} supported={} ready={} redirect={} xsk={}/{} tcp={}",
            expected_ports.len(),
            status.attached,
            status.proxy_ports,
            status.proxy_supported_ports,
            status.proxy_ready,
            status.proxy_redirect_enabled,
            status.xsk_ready_queues,
            status.xsk_configured_queues,
            status.tcp_dataplane_ready,
        )));
    }
    Ok(())
}

fn phase_order(phase: &str) -> Option<u8> {
    match phase {
        "prepare" => Some(1),
        "validate" => Some(2),
        "apply" => Some(3),
        "ack" => Some(4),
        _ => None,
    }
}

/// complete_release confirms a verified release with the API by driving the
/// ApplyRelease phase vocabulary in order. Only called after the live config
/// store actually carries the verified version: `active_version` must equal
/// `release.version` (and be positive) before any RPC is sent. A failed call
/// stops the sequence — the next sync round retries from the persisted stage.
/// ApplyReleaseSender is the test seam for `complete_release`: production
/// uses the real tonic NodeControl client; tests substitute a fake that
/// records or fails calls without touching RpcClient or vendor code.
pub trait ApplyReleaseSender {
    fn apply_release(
        &mut self,
        req: ApplyReleaseRequest,
    ) -> impl std::future::Future<Output = Result<super::proto::ApplyReleaseResponse, Status>> + Send + '_;
}

impl<F> ApplyReleaseSender
    for super::proto::node_control_client::NodeControlClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, F>,
    >
where
    F: FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, Status> + Clone + Send + 'static,
{
    async fn apply_release(
        &mut self,
        req: ApplyReleaseRequest,
    ) -> Result<super::proto::ApplyReleaseResponse, Status> {
        crate::rpc::track_rpc(self.apply_release(req))
            .await
            .map(tonic::Response::into_inner)
    }
}

fn check_release_version(release: &VerifiedRelease, active_version: i64) -> Result<(), Status> {
    if active_version <= 0 {
        return Err(Status::failed_precondition(
            "release confirmation requires a positive active config version",
        ));
    }
    if active_version != release.version {
        return Err(Status::failed_precondition(
            "release confirmation version mismatch with active config",
        ));
    }
    Ok(())
}

pub async fn complete_release(
    api: &ApiConfig,
    release: &VerifiedRelease,
    active_version: i64,
) -> Result<(), Status> {
    check_release_version(release, active_version)?;
    let rpc = crate::rpc::client::SharedRpcClient::get(api)
        .await
        .map_err(|err| Status::unavailable(err.to_string()))?
        .as_rpc_client();
    let mut client = rpc.node_control_service();
    complete_release_with(api, release, active_version, &mut client).await
}

/// complete_release_with is the testable core of `complete_release`: `sender`
/// performs one ApplyRelease RPC per phase. Phase order and per-response
/// validation are enforced here so a fake cannot skip stages or forge
/// vocabulary.
pub(crate) async fn complete_release_with<S>(
    api: &ApiConfig,
    release: &VerifiedRelease,
    active_version: i64,
    sender: &mut S,
) -> Result<(), Status>
where
    S: ApplyReleaseSender,
{
    check_release_version(release, active_version)?;
    for phase in ["prepare", "validate", "apply", "ack"] {
        let want = phase_order(phase).expect("phase vocabulary is fixed");
        let resp = sender
            .apply_release(ApplyReleaseRequest {
                meta: Some(EnvelopeMeta {
                    schema_version: "node.v1".into(),
                    min_reader_version: "node.v1".into(),
                    base_version: release.version,
                    patch_seq: 0,
                }),
                node_id: api.node_id.clone(),
                release_id: release.release_id.clone(),
                phase: phase.to_string(),
            })
            .await?;
        // Response meta is mandatory on this path: schema/min-reader must be
        // node.v1 and base_version must echo the frozen release version.
        let meta = resp
            .meta
            .ok_or_else(|| Status::data_loss("NodeControl apply response metadata missing"))?;
        if meta.schema_version != "node.v1" || meta.min_reader_version != "node.v1" {
            return Err(Status::failed_precondition(
                "unsupported NodeControl apply schema",
            ));
        }
        if meta.base_version != release.version || meta.patch_seq != 0 {
            return Err(Status::failed_precondition(
                "NodeControl apply response version mismatch",
            ));
        }
        // The response carries the caller node's persisted phase. A returned
        // phase ahead of the request is only valid as an idempotent replay;
        // regressions or unknown vocabulary are rejected.
        let persisted = phase_order(&resp.phase_status)
            .ok_or_else(|| Status::data_loss("NodeControl apply returned an unknown phase"))?;
        if persisted < want {
            return Err(Status::data_loss(
                "NodeControl apply phase regressed below the requested stage",
            ));
        }
        if persisted > want && !resp.idempotent_replay {
            return Err(Status::data_loss(
                "NodeControl apply phase advanced past the requested stage without replay",
            ));
        }
    }
    Ok(())
}

fn envelope(version: i64) -> EnvelopeMeta {
    EnvelopeMeta {
        schema_version: "node.v1".into(),
        min_reader_version: "node.v1".into(),
        base_version: version,
        patch_seq: 0,
    }
}

pub async fn report_health(
    api: &ApiConfig,
    version: i64,
    status: &serde_json::Value,
) -> Result<(), Status> {
    let rpc = crate::rpc::client::SharedRpcClient::get(api)
        .await
        .map_err(|err| Status::unavailable(err.to_string()))?
        .as_rpc_client();
    let response = crate::rpc::track_rpc(rpc.node_control_service().report_node_health(ReportNodeHealthRequest {
        meta:Some(envelope(version)),node_id:api.node_id.clone(),ready:status.get("isHealthy").and_then(|value|value.as_bool()).unwrap_or(false),
        listeners_json:serde_json::json!({"connections":status["connectionCount"],"transports":status["connectionShadow"]}).to_string(),
        cache_json:serde_json::json!({"diskBytes":status["cacheTotalDiskSize"],"memoryBytes":status["cacheTotalMemorySize"]}).to_string(),
        xdp_json:status["xdp"].to_string(),resource_pressure_json:status["resourceGovernor"].to_string(),task_status_json:status["pipelines"].to_string(),
    })).await?.into_inner();
    if !response.accepted {
        return Err(Status::failed_precondition("NodeControl health rejected"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    use hmac::Mac;

    fn api() -> ApiConfig {
        ApiConfig {
            rpc_tls: Default::default(),
            rpc_endpoints: vec![],
            rpc_disable_update: false,
            node_id: "nd_test".into(),
            secret: "secret".into(),
            access_log_pipeline: Default::default(),
            relay: Default::default(),
            kernel_tuning: Default::default(),
            cache: Default::default(),
        }
    }

    fn meta(version: i64) -> EnvelopeMeta {
        EnvelopeMeta {
            schema_version: "node.v1".into(),
            min_reader_version: "node.v1".into(),
            base_version: version,
            patch_seq: 0,
        }
    }

    fn snapshot_response(
        version: i64,
        required: bool,
        body: &[u8],
        secret: &str,
    ) -> crate::node_control::proto::GetDesiredReleaseResponse {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&version.to_be_bytes());
        mac.update(body);
        crate::node_control::proto::GetDesiredReleaseResponse {
            meta: Some(meta(version)),
            release_id: String::new(),
            signature_digest: vec![],
            snapshot_required: required,
            snapshot_json: body.to_vec(),
            snapshot_hmac: if required {
                mac.finalize().into_bytes().to_vec()
            } else {
                vec![]
            },
        }
    }

    fn pin_key(pair: &Ed25519KeyPair) {
        crate::api_config::install_release_signing_key(&crate::api_config::ReleaseSigningPush {
            algorithm: "ed25519".into(),
            public_key: hex::encode(pair.public_key().as_ref()),
        });
    }

    fn keypair() -> Ed25519KeyPair {
        let doc = Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap()
    }

    // The pinned-key slot is process-global; serialize tests that mutate it.
    use crate::api_config::RELEASE_SIGNING_TEST_LOCK as KEY_LOCK;

    #[test]
    fn signature_binds_version_payload_and_node_secret() {
        let payload = br#"{"version":7}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(&7_i64.to_be_bytes());
        mac.update(payload);
        let signature = mac.finalize().into_bytes();
        assert!(verify_snapshot("secret", 7, payload, &signature).is_ok());
        assert!(verify_snapshot("secret", 8, payload, &signature).is_err());
        assert!(verify_snapshot("other", 7, payload, &signature).is_err());
        assert!(verify_snapshot("secret", 7, b"{}", &signature).is_err());
    }

    #[test]
    fn snapshot_verification_rejects_tampering() {
        let _g = KEY_LOCK.blocking_lock();
        crate::rpc::client::SharedRpcClient::reset_for_test();
        crate::api_config::reset_release_signing_key();
        let api = api();
        let body = br#"{"version":7}"#.as_slice();
        // missing meta
        let mut resp = snapshot_response(7, true, body, "secret");
        resp.meta = None;
        assert!(verify_snapshot_response(&api, 7, resp).is_err());
        // wrong schema
        let mut resp = snapshot_response(7, true, body, "secret");
        resp.meta.as_mut().unwrap().schema_version = "node.v0".into();
        assert!(verify_snapshot_response(&api, 7, resp).is_err());
        // wrong HMAC secret
        let resp = snapshot_response(7, true, body, "other");
        assert!(verify_snapshot_response(&api, 7, resp).is_err());
        // embedded doc version mismatch
        let resp = snapshot_response(8, true, body, "secret");
        assert!(verify_snapshot_response(&api, 8, resp).is_err());
        // unchanged response with wrong base version
        let resp = snapshot_response(9, false, b"", "secret");
        assert!(verify_snapshot_response(&api, 7, resp).is_err());
        // unchanged response carrying unsigned payload bytes → reject
        let mut resp = snapshot_response(7, false, b"", "secret");
        resp.snapshot_json = br#"{"version":7}"#.to_vec();
        assert_eq!(
            verify_snapshot_response(&api, 7, resp).unwrap_err().code(),
            tonic::Code::DataLoss
        );
        // unchanged response carrying a stray HMAC → reject
        let mut resp = snapshot_response(7, false, b"", "secret");
        resp.snapshot_hmac = vec![1; 32];
        assert_eq!(
            verify_snapshot_response(&api, 7, resp).unwrap_err().code(),
            tonic::Code::DataLoss
        );
        // nonzero patch_seq is not a valid full-snapshot response
        let mut resp = snapshot_response(7, false, b"", "secret");
        resp.meta.as_mut().unwrap().patch_seq = 1;
        assert_eq!(
            verify_snapshot_response(&api, 7, resp).unwrap_err().code(),
            tonic::Code::DataLoss
        );
        // valid unchanged response passes
        let resp = snapshot_response(7, false, b"", "secret");
        let snap = verify_snapshot_response(&api, 7, resp).unwrap();
        assert!(!snap.config.is_changed);
        assert!(snap.release.is_none());
    }

    #[test]
    fn release_binding_requires_key_and_signature() {
        let _g = KEY_LOCK.blocking_lock();
        crate::rpc::client::SharedRpcClient::reset_for_test();
        crate::api_config::reset_release_signing_key();
        let api = api();
        let pair = keypair();
        let mut resp = snapshot_response(7, true, br#"{"version":7}"#, "secret");
        resp.release_id = "rel-9".into();
        resp.signature_digest = pair.sign(b"rel-9").as_ref().to_vec();
        // no pinned key → reject
        assert_eq!(
            verify_snapshot_response(&api, 7, resp.clone())
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        // pinned key, empty digest → reject
        pin_key(&pair);
        let mut resp = snapshot_response(7, true, br#"{"version":7}"#, "secret");
        resp.release_id = "rel-9".into();
        assert_eq!(
            verify_snapshot_response(&api, 7, resp).unwrap_err().code(),
            tonic::Code::DataLoss
        );
        // wrong signature → reject
        let mut resp = snapshot_response(7, true, br#"{"version":7}"#, "secret");
        resp.release_id = "rel-9".into();
        resp.signature_digest = pair.sign(b"other").as_ref().to_vec();
        assert!(verify_snapshot_response(&api, 7, resp).is_err());
        // valid signature + unchanged snapshot still preserves the release
        let mut resp = snapshot_response(7, false, b"", "secret");
        resp.release_id = "rel-9".into();
        resp.signature_digest = pair.sign(b"rel-9").as_ref().to_vec();
        let snap = verify_snapshot_response(&api, 7, resp).unwrap();
        let release = snap.release.expect("verified release preserved");
        assert_eq!(release.release_id, "rel-9");
        assert_eq!(release.version, 7);
        // nonpositive frozen version rejects
        let mut resp = snapshot_response(0, false, b"", "secret");
        resp.release_id = "rel-9".into();
        resp.signature_digest = pair.sign(b"rel-9").as_ref().to_vec();
        assert!(verify_snapshot_response(&api, 0, resp).is_err());
    }

    fn resp_for(phase: &str) -> crate::node_control::proto::ApplyReleaseResponse {
        resp_for_replay(phase, false)
    }

    fn resp_for_replay(
        phase: &str,
        replay: bool,
    ) -> crate::node_control::proto::ApplyReleaseResponse {
        crate::node_control::proto::ApplyReleaseResponse {
            meta: Some(meta(7)),
            idempotent_replay: replay,
            phase_status: phase.into(),
        }
    }

    struct FakeSender {
        calls: Vec<ApplyReleaseRequest>,
        fail_at: Option<&'static str>,
        respond: &'static str,
        echo: bool,
        replay: bool,
    }

    impl ApplyReleaseSender for FakeSender {
        fn apply_release(
            &mut self,
            req: ApplyReleaseRequest,
        ) -> impl std::future::Future<
            Output = Result<crate::node_control::proto::ApplyReleaseResponse, Status>,
        > + Send
        + '_ {
            let phase = req.phase.clone();
            self.calls.push(req);
            async move {
                if self.fail_at == Some(phase.as_str()) {
                    return Err(Status::unavailable("simulated"));
                }
                Ok(if self.echo {
                    resp_for(&phase)
                } else {
                    resp_for_replay(self.respond, self.replay)
                })
            }
        }
    }

    #[tokio::test]
    async fn complete_release_drives_phases_in_order() {
        let api = api();
        let release = VerifiedRelease {
            release_id: "rel-9".into(),
            version: 7,
        };
        let mut sender = FakeSender {
            calls: vec![],
            fail_at: None,
            respond: "",
            echo: true,
            replay: false,
        };
        complete_release_with(&api, &release, 7, &mut sender)
            .await
            .unwrap();
        let phases: Vec<&str> = sender.calls.iter().map(|r| r.phase.as_str()).collect();
        assert_eq!(phases, ["prepare", "validate", "apply", "ack"]);
        for req in &sender.calls {
            assert_eq!(req.release_id, "rel-9");
            assert_eq!(req.node_id, "nd_test");
            assert_eq!(req.meta.as_ref().unwrap().base_version, 7);
            assert_eq!(req.meta.as_ref().unwrap().schema_version, "node.v1");
        }
    }

    #[tokio::test]
    async fn complete_release_rejects_version_mismatch_without_rpc() {
        let api = api();
        let release = VerifiedRelease {
            release_id: "rel-9".into(),
            version: 7,
        };
        for active in [0, 8] {
            let mut sender = FakeSender {
                calls: vec![],
                fail_at: None,
                respond: "",
                echo: true,
                replay: false,
            };
            assert!(
                complete_release_with(&api, &release, active, &mut sender)
                    .await
                    .is_err()
            );
            assert!(sender.calls.is_empty());
        }
    }

    #[tokio::test]
    async fn complete_release_stops_before_ack_on_failure() {
        let api = api();
        let release = VerifiedRelease {
            release_id: "rel-9".into(),
            version: 7,
        };
        let mut sender = FakeSender {
            calls: vec![],
            fail_at: Some("ack"),
            respond: "",
            echo: true,
            replay: false,
        };
        let err = complete_release_with(&api, &release, 7, &mut sender)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
        let phases: Vec<&str> = sender.calls.iter().map(|r| r.phase.as_str()).collect();
        assert_eq!(phases, ["prepare", "validate", "apply", "ack"]);
    }

    #[tokio::test]
    async fn complete_release_rejects_regressed_or_unknown_phase() {
        let api = api();
        let release = VerifiedRelease {
            release_id: "rel-9".into(),
            version: 7,
        };
        // regressed persisted phase
        let mut sender = FakeSender {
            calls: vec![],
            fail_at: None,
            respond: "prepare",
            echo: false,
            replay: false,
        };
        let err = complete_release_with(&api, &release, 7, &mut sender)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::DataLoss);
        // unknown phase vocabulary
        let mut sender = FakeSender {
            calls: vec![],
            fail_at: None,
            respond: "bogus",
            echo: false,
            replay: false,
        };
        let err = complete_release_with(&api, &release, 7, &mut sender)
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::DataLoss);
        // idempotent replay of the persisted stage is accepted
        let mut sender = FakeSender {
            calls: vec![],
            fail_at: None,
            respond: "ack",
            echo: false,
            replay: true,
        };
        complete_release_with(&api, &release, 7, &mut sender)
            .await
            .unwrap();
    }
}

// Loopback NodeControl server driving the real fetch_snapshot →
// fetch_and_apply_config → complete_release chain over an ephemeral local
// channel. Unit-only: no production process is involved.
#[cfg(test)]
mod loopback_tests {
    use super::*;
    use crate::node_control::proto;
    use crate::node_control::proto::node_control_server::{NodeControl, NodeControlServer};
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::sync::{Arc, Mutex};
    use tonic::transport::{Channel, Server};
    use tonic::{Request, Response};

    type Recv<T> = tonic::codegen::tokio_stream::wrappers::ReceiverStream<T>;

    struct FakeControl {
        calls: Mutex<Vec<String>>,
        stages: Mutex<Vec<String>>,
        fail_once: Mutex<Option<String>>,
        desired: Mutex<Option<proto::GetDesiredReleaseResponse>>,
    }

    impl FakeControl {
        fn order(p: &str) -> u8 {
            match p {
                "prepare" => 1,
                "validate" => 2,
                "apply" => 3,
                "ack" => 4,
                _ => 0,
            }
        }
    }

    #[tonic::async_trait]
    impl NodeControl for FakeControl {
        async fn register_node(
            &self,
            _r: Request<proto::RegisterNodeRequest>,
        ) -> Result<Response<proto::RegisterNodeResponse>, Status> {
            Ok(Response::new(proto::RegisterNodeResponse {
                meta: Some(EnvelopeMeta {
                    schema_version: "node.v1".into(),
                    min_reader_version: "node.v1".into(),
                    base_version: 0,
                    patch_seq: 0,
                }),
                accepted: true,
                enrollment_status: "ok".into(),
            }))
        }

        async fn get_desired_release(
            &self,
            _r: Request<proto::GetDesiredReleaseRequest>,
        ) -> Result<Response<proto::GetDesiredReleaseResponse>, Status> {
            match self.desired.lock().unwrap().clone() {
                Some(resp) => Ok(Response::new(resp)),
                None => Err(Status::unimplemented("no desired release")),
            }
        }

        async fn apply_release(
            &self,
            r: Request<proto::ApplyReleaseRequest>,
        ) -> Result<Response<proto::ApplyReleaseResponse>, Status> {
            let req = r.into_inner();
            let phase = req.phase.clone();
            self.calls.lock().unwrap().push(phase.clone());
            let mut fail = self.fail_once.lock().unwrap();
            if fail.as_deref() == Some(phase.as_str()) {
                *fail = None;
                drop(fail);
                return Err(Status::unavailable("simulated apply failure"));
            }
            drop(fail);
            let want = Self::order(&phase);
            if want == 0 {
                return Err(Status::invalid_argument("bad phase"));
            }
            let mut stages = self.stages.lock().unwrap();
            let current = stages.last().cloned();
            let cur_order = current.as_deref().map(Self::order).unwrap_or(0);
            if want > cur_order + 1 {
                return Err(Status::invalid_argument("phase skip"));
            }
            let (phase_status, replay) = if want <= cur_order {
                (current.unwrap(), true)
            } else {
                stages.push(phase.clone());
                (phase, false)
            };
            Ok(Response::new(proto::ApplyReleaseResponse {
                meta: Some(req.meta.clone().unwrap_or(EnvelopeMeta {
                    schema_version: "node.v1".into(),
                    min_reader_version: "node.v1".into(),
                    base_version: 0,
                    patch_seq: 0,
                })),
                idempotent_replay: replay,
                phase_status,
            }))
        }

        type StreamEventsStream = Recv<Result<proto::StreamEventsResponse, Status>>;
        async fn stream_events(
            &self,
            _r: Request<tonic::Streaming<proto::StreamEventsRequest>>,
        ) -> Result<Response<Self::StreamEventsStream>, Status> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(Response::new(Recv::new(rx)))
        }

        async fn report_node_health(
            &self,
            _r: Request<proto::ReportNodeHealthRequest>,
        ) -> Result<Response<proto::ReportNodeHealthResponse>, Status> {
            Ok(Response::new(proto::ReportNodeHealthResponse {
                meta: None,
                accepted: true,
            }))
        }
    }

    struct Harness {
        api: ApiConfig,
        fake: Arc<FakeControl>,
    }

    async fn harness(pair: &Ed25519KeyPair, desired: proto::GetDesiredReleaseResponse) -> Harness {
        let fake = Arc::new(FakeControl {
            calls: Mutex::new(vec![]),
            stages: Mutex::new(vec![]),
            fail_once: Mutex::new(None),
            desired: Mutex::new(Some(desired)),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = NodeControlServer::from_arc(fake.clone())
            .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
            .send_compressed(tonic::codec::CompressionEncoding::Gzip);
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(server)
                .serve_with_incoming(
                    tonic::codegen::tokio_stream::wrappers::TcpListenerStream::new(listener),
                )
                .await;
        });
        crate::rpc::client::SharedRpcClient::reset_for_test();
        crate::api_config::reset_release_signing_key();
        crate::api_config::install_release_signing_key(&crate::api_config::ReleaseSigningPush {
            algorithm: "ed25519".into(),
            public_key: hex::encode(pair.public_key().as_ref()),
        });
        Harness {
            api: ApiConfig {
                rpc_tls: Default::default(),
                rpc_endpoints: vec![format!("http://{addr}")],
                rpc_disable_update: false,
                node_id: "nd_test".into(),
                secret: "secret".into(),
                access_log_pipeline: Default::default(),
                relay: Default::default(),
                kernel_tuning: Default::default(),
                cache: Default::default(),
            },
            fake,
        }
    }

    fn desired_response(
        pair: &Ed25519KeyPair,
        version: i64,
        body: &[u8],
        required: bool,
    ) -> proto::GetDesiredReleaseResponse {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(&version.to_be_bytes());
        mac.update(body);
        proto::GetDesiredReleaseResponse {
            meta: Some(EnvelopeMeta {
                schema_version: "node.v1".into(),
                min_reader_version: "node.v1".into(),
                base_version: version,
                patch_seq: 0,
            }),
            release_id: "rel-1".into(),
            signature_digest: pair.sign(b"rel-1").as_ref().to_vec(),
            snapshot_required: required,
            snapshot_json: body.to_vec(),
            snapshot_hmac: if required {
                mac.finalize().into_bytes().to_vec()
            } else {
                vec![]
            },
        }
    }

    fn keypair() -> Ed25519KeyPair {
        let doc = Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap()
    }

    #[tokio::test]
    async fn fetch_snapshot_preserves_verified_release_over_loopback() {
        let _g = crate::api_config::RELEASE_SIGNING_TEST_LOCK.lock().await;
        let pair = keypair();
        let h = harness(&pair, desired_response(&pair, 7, br#"{"version":7}"#, true)).await;
        let snap = fetch_snapshot(&h.api, 7).await.unwrap();
        assert!(snap.config.is_changed);
        let release = snap.release.expect("verified release preserved");
        assert_eq!(release.release_id, "rel-1");
        assert_eq!(release.version, 7);
    }

    #[tokio::test]
    async fn fetch_apply_failure_never_reaches_apply_release() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let _g = crate::api_config::RELEASE_SIGNING_TEST_LOCK.lock().await;
        let pair = keypair();
        // valid HMAC+signature but the snapshot payload decodes badly
        let h = harness(
            &pair,
            desired_response(&pair, 7, br#"{"version":7,"servers":42}"#, true),
        )
        .await;
        let channel = Channel::from_shared(h.api.rpc_endpoints[0].clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut legacy = crate::pb::node_service_client::NodeServiceClient::with_interceptor(
            channel,
            |req: Request<()>| Ok(req),
        );
        let store = crate::config::ConfigStore::new();
        store.update_id(5).await;
        store.update_config_version(3).await;
        let health = crate::health_manager::GlobalHealthManager::new(1);
        let selector = crate::ssl::DynamicCertSelector::new();
        let waf = crate::firewall::state::WafStateManager::new();
        let mut task_v = 0i64;
        let mut cfg_v = 3i64;
        let ok = crate::rpc::node::fetch_and_apply_config(crate::rpc::node::FetchConfigArgs {
            client: &mut legacy,
            config_store: &store,
            api_config: &h.api,
            health_manager: &health,
            cert_selector: &selector,
            waf_state: &waf,
            task_version: &mut task_v,
            config_version: &mut cfg_v,
        })
        .await;
        assert!(!ok, "decode failure must return false");
        assert!(
            h.fake.calls.lock().unwrap().is_empty(),
            "no release phase may be sent after an apply failure"
        );
        assert_eq!(
            store.get_config_version().await,
            3,
            "failed apply must preserve the old version"
        );
    }

    #[tokio::test]
    async fn unchanged_version_drives_release_phases_and_retries() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .try_init();
        let _g = crate::api_config::RELEASE_SIGNING_TEST_LOCK.lock().await;
        let pair = keypair();
        let h = harness(&pair, desired_response(&pair, 7, b"", false)).await;
        let channel = Channel::from_shared(h.api.rpc_endpoints[0].clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut legacy = crate::pb::node_service_client::NodeServiceClient::with_interceptor(
            channel,
            |req: Request<()>| Ok(req),
        );
        let store = crate::config::ConfigStore::new();
        store.update_id(5).await;
        store.update_config_version(7).await;
        let health = crate::health_manager::GlobalHealthManager::new(1);
        let selector = crate::ssl::DynamicCertSelector::new();
        let waf = crate::firewall::state::WafStateManager::new();
        let mut task_v = 0i64;
        let mut cfg_v = 7i64;
        // unchanged snapshot at the frozen version → 4 phases over the real RPC
        assert!(
            crate::rpc::node::fetch_and_apply_config(crate::rpc::node::FetchConfigArgs {
                client: &mut legacy,
                config_store: &store,
                api_config: &h.api,
                health_manager: &health,
                cert_selector: &selector,
                waf_state: &waf,
                task_version: &mut task_v,
                config_version: &mut cfg_v,
            })
            .await
        );
        assert_eq!(
            h.fake.calls.lock().unwrap().as_slice(),
            ["prepare", "validate", "apply", "ack"]
        );

        // failure after apply halts before persisting ack; a retry with the
        // unchanged version replays completed stages and acks once.
        let pair2 = keypair();
        let h2 = harness(&pair2, desired_response(&pair2, 7, b"", false)).await;
        *h2.fake.fail_once.lock().unwrap() = Some("ack".into());
        let channel2 = Channel::from_shared(h2.api.rpc_endpoints[0].clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut legacy2 = crate::pb::node_service_client::NodeServiceClient::with_interceptor(
            channel2,
            |req: Request<()>| Ok(req),
        );
        let store2 = crate::config::ConfigStore::new();
        store2.update_id(5).await;
        store2.update_config_version(7).await;
        let health2 = crate::health_manager::GlobalHealthManager::new(1);
        let selector2 = crate::ssl::DynamicCertSelector::new();
        let waf2 = crate::firewall::state::WafStateManager::new();
        let mut tv2 = 0i64;
        let mut cv2 = 7i64;
        assert!(
            !crate::rpc::node::fetch_and_apply_config(crate::rpc::node::FetchConfigArgs {
                client: &mut legacy2,
                config_store: &store2,
                api_config: &h2.api,
                health_manager: &health2,
                cert_selector: &selector2,
                waf_state: &waf2,
                task_version: &mut tv2,
                config_version: &mut cv2,
            })
            .await,
            "ack RPC failure must return false"
        );
        assert!(
            crate::rpc::node::fetch_and_apply_config(crate::rpc::node::FetchConfigArgs {
                client: &mut legacy2,
                config_store: &store2,
                api_config: &h2.api,
                health_manager: &health2,
                cert_selector: &selector2,
                waf_state: &waf2,
                task_version: &mut tv2,
                config_version: &mut cv2,
            })
            .await,
            "retry with unchanged version must complete"
        );
        let calls = h2.fake.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            [
                "prepare", "validate", "apply", "ack", "prepare", "validate", "apply", "ack"
            ]
        );
    }

    #[tokio::test]
    async fn fallback_decision_only_unimplemented() {
        use crate::node_control::transport::uses_legacy_fallback;
        assert!(uses_legacy_fallback(&Status::unimplemented("old api")));
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::DataLoss,
            tonic::Code::FailedPrecondition,
            tonic::Code::Unauthenticated,
        ] {
            assert!(!uses_legacy_fallback(&Status::new(code, "x")));
        }
    }

    fn ready_xdp_status() -> crate::xdp::XdpStatusSnapshot {
        crate::xdp::XdpStatusSnapshot {
            enabled: true,
            available: true,
            attached: true,
            proxy_ports: 3,
            proxy_supported_ports: 3,
            proxy_ready: true,
            proxy_redirect_enabled: true,
            xsk_configured_queues: 2,
            xsk_ready_queues: 2,
            tcp_dataplane_ready: true,
            ..Default::default()
        }
    }

    fn http_ports() -> Vec<crate::runtime_mode::XdpProxyPortConfig> {
        vec![
            crate::runtime_mode::XdpProxyPortConfig {
                protocol: crate::runtime_mode::XdpProxyProtocol::Http,
                port: 80,
            },
            crate::runtime_mode::XdpProxyPortConfig {
                protocol: crate::runtime_mode::XdpProxyProtocol::Https,
                port: 443,
            },
        ]
    }

    #[test]
    fn release_dataplane_guard_rejects_each_unready_bit() {
        let ports = http_ports();
        // every degradation independently fails closed
        for mutate in [
            |s: &mut crate::xdp::XdpStatusSnapshot| s.available = false,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.attached = false,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.proxy_ports = 0,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.proxy_supported_ports = 0,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.proxy_ready = false,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.proxy_redirect_enabled = false,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.xsk_configured_queues = 0,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.xsk_ready_queues = 1,
            |s: &mut crate::xdp::XdpStatusSnapshot| s.tcp_dataplane_ready = false,
        ] {
            let mut status = ready_xdp_status();
            mutate(&mut status);
            assert!(
                validate_release_dataplane_snapshot(&status, &ports).is_err(),
                "mutated status must not confirm"
            );
        }
        // fully ready accepts
        assert!(validate_release_dataplane_snapshot(&ready_xdp_status(), &ports).is_ok());
        // bypass: XDP disabled or no active business ports
        let mut off = ready_xdp_status();
        off.enabled = false;
        assert!(validate_release_dataplane_snapshot(&off, &ports).is_ok());
        let mut degraded = ready_xdp_status();
        degraded.attached = false;
        assert!(validate_release_dataplane_snapshot(&degraded, &[]).is_ok());
        // udp-only business does not require the tcp dataplane
        let udp_only = vec![crate::runtime_mode::XdpProxyPortConfig {
            protocol: crate::runtime_mode::XdpProxyProtocol::Udp,
            port: 53,
        }];
        let mut no_tcp = ready_xdp_status();
        no_tcp.tcp_dataplane_ready = false;
        assert!(validate_release_dataplane_snapshot(&no_tcp, &udp_only).is_ok());
    }
}
