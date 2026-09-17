//! Structured ReportNodeHealth fields (not a single opaque statusJSON blob).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeHealthReport {
    pub ready: bool,
    pub listeners_json: String,
    pub cache_json: String,
    pub xdp_json: String,
    pub resource_pressure_json: String,
    pub task_status_json: String,
}

impl NodeHealthReport {
    pub fn validate_json_fields(&self) -> Result<(), String> {
        for (name, raw) in [
            ("listeners_json", self.listeners_json.as_str()),
            ("cache_json", self.cache_json.as_str()),
            ("xdp_json", self.xdp_json.as_str()),
            (
                "resource_pressure_json",
                self.resource_pressure_json.as_str(),
            ),
            ("task_status_json", self.task_status_json.as_str()),
        ] {
            if raw.trim().is_empty() {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(raw)
                .map_err(|err| format!("{name}: {err}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_fields_roundtrip() {
        let report = NodeHealthReport {
            ready: true,
            listeners_json: r#"[{"proto":"https","port":443}]"#.into(),
            cache_json: r#"{"disk_bytes":1}"#.into(),
            xdp_json: r#"{"enabled":false}"#.into(),
            resource_pressure_json: r#"{"cpu":0.1}"#.into(),
            task_status_json: r#"{"apply":"idle"}"#.into(),
        };
        report.validate_json_fields().unwrap();
        let encoded = serde_json::to_string(&report).unwrap();
        let decoded: NodeHealthReport = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, report);
    }
}
