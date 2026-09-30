//! The migration transport shares the exact legacy snapshot apply pipeline.
//! Unsupported old APIs retain the legacy protocol; other failures never fall back.
use super::proto::{
    EnvelopeMeta, GetDesiredReleaseRequest, RegisterNodeRequest, ReportNodeHealthRequest,
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

pub async fn fetch_snapshot(
    api: &ApiConfig,
    version: i64,
) -> Result<tonic::Response<pb::FindCurrentNodeConfigResponse>, Status> {
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
        software_version: env!("CARGO_PKG_VERSION").into(),
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
    } else if meta.base_version != version {
        return Err(Status::data_loss(
            "NodeControl version changed without snapshot",
        ));
    }
    Ok(tonic::Response::new(pb::FindCurrentNodeConfigResponse {
        is_changed: response.snapshot_required,
        is_compressed: false,
        data_size: response.snapshot_json.len() as i64,
        node_json: response.snapshot_json,
        timestamp: meta.base_version,
    }))
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
}
