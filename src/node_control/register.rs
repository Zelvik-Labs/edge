//! NodeControl.RegisterNode input checks (not IndexService.registerNode).

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommMode {
    Plaintext,
    Tls,
    Mtls,
}

impl CommMode {
    pub fn parse(raw: &str) -> Result<Self, RegisterError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "plaintext" => Ok(Self::Plaintext),
            "tls" => Ok(Self::Tls),
            "mtls" => Ok(Self::Mtls),
            other => Err(RegisterError::UnknownCommMode(other.to_string())),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plaintext => "plaintext",
            Self::Tls => "tls",
            Self::Mtls => "mtls",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RegisterInput {
    pub node_id: String,
    pub software_version: String,
    pub capabilities_json: String,
    pub region: String,
    pub comm_mode: String,
    pub cert_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterOutcome {
    pub accepted: bool,
    pub enrollment_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    MissingNodeId,
    MissingSoftwareVersion,
    InvalidCapabilitiesJson,
    UnknownCommMode(String),
    /// Index-shaped payload (node group / IP only) is not NodeControl enrollment.
    IndexShapedPayload,
}

pub fn enroll(input: &RegisterInput) -> Result<RegisterOutcome, RegisterError> {
    if input.node_id.trim().is_empty() {
        return Err(RegisterError::MissingNodeId);
    }
    if looks_like_index_register(input) {
        return Err(RegisterError::IndexShapedPayload);
    }
    if input.software_version.trim().is_empty() {
        return Err(RegisterError::MissingSoftwareVersion);
    }
    CommMode::parse(&input.comm_mode)?;
    if !input.capabilities_json.trim().is_empty() {
        serde_json::from_str::<Value>(&input.capabilities_json)
            .map_err(|_| RegisterError::InvalidCapabilitiesJson)?;
    }
    Ok(RegisterOutcome {
        accepted: true,
        enrollment_status: "enrolled".to_string(),
    })
}

/// IndexService.registerNode only had nodeId / nodeGroupId / nodeIP.
fn looks_like_index_register(input: &RegisterInput) -> bool {
    input.software_version.is_empty()
        && input.capabilities_json.is_empty()
        && input.comm_mode.is_empty()
        && input.cert_fingerprint.is_empty()
        && input.region.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !input.region.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> RegisterInput {
        RegisterInput {
            node_id: "node-a".into(),
            software_version: "2026.1.08.0".into(),
            capabilities_json: r#"{"http3":true,"xdp":true}"#.into(),
            region: "cn-east".into(),
            comm_mode: "tls".into(),
            cert_fingerprint: "sha256:deadbeef".into(),
        }
    }

    #[test]
    fn enrolls_node_control_fields() {
        let out = enroll(&valid()).unwrap();
        assert!(out.accepted);
        assert_eq!(out.enrollment_status, "enrolled");
    }

    #[test]
    fn rejects_index_shaped_payload() {
        let input = RegisterInput {
            node_id: "1".into(),
            software_version: String::new(),
            capabilities_json: String::new(),
            region: "10.0.0.1".into(),
            comm_mode: String::new(),
            cert_fingerprint: String::new(),
        };
        assert_eq!(enroll(&input), Err(RegisterError::IndexShapedPayload));
    }

    #[test]
    fn rejects_unknown_comm_mode() {
        let mut input = valid();
        input.comm_mode = "switch".into();
        assert!(matches!(
            enroll(&input),
            Err(RegisterError::UnknownCommMode(_))
        ));
    }
}
