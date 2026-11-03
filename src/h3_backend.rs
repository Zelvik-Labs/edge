//! Explicit experimental transport selection; the default remains shared Quinn.
use anyhow::{Result, bail};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H3Backend {
    QuinnShared,
    QuinnNative,
    TokioQuicheNative,
}
fn parse(value: &str) -> Result<H3Backend> {
    match value {
        "quinn-shared" => Ok(H3Backend::QuinnShared),
        "quinn-native" => Ok(H3Backend::QuinnNative),
        "tokio-quiche-native" => {
            if cfg!(feature = "h3-quiche") {
                Ok(H3Backend::TokioQuicheNative)
            } else {
                bail!("tokio-quiche-native requires a build with --features h3-quiche")
            }
        }
        _ => bail!(
            "unknown ZELVIK_EDGE_H3_BACKEND: {value}; expected quinn-shared, quinn-native or tokio-quiche-native"
        ),
    }
}
static SELECTED: LazyLock<Result<H3Backend, String>> = LazyLock::new(|| {
    let value = match std::env::var("ZELVIK_EDGE_H3_BACKEND") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(H3Backend::QuinnShared),
        Err(error) => return Err(error.to_string()),
    };
    parse(&value).map_err(|error| error.to_string())
});
pub fn selected() -> Result<H3Backend> {
    SELECTED
        .as_ref()
        .copied()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

pub fn validate_runtime(runtime: &crate::runtime_mode::RuntimeConfig) -> Result<()> {
    let profile = transport_profile()?;
    anyhow::ensure!(
        selected()? != H3Backend::TokioQuicheNative || profile == TransportProfile::Matched,
        "tokio-quiche-native requires ZELVIK_EDGE_H3_TRANSPORT_PROFILE=matched for the experimental bounded transport"
    );
    anyhow::ensure!(
        selected()? == H3Backend::QuinnShared || !runtime.xdp.enabled,
        "native H3 experiment requires kernel UDP; XDP must use the shared Quinn backend"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportProfile {
    Production,
    Matched,
}
static PROFILE: LazyLock<Result<TransportProfile, String>> =
    LazyLock::new(|| match std::env::var("ZELVIK_EDGE_H3_TRANSPORT_PROFILE") {
        Ok(value) if value == "production" => Ok(TransportProfile::Production),
        Ok(value) if value == "matched" => Ok(TransportProfile::Matched),
        Err(std::env::VarError::NotPresent) => Ok(TransportProfile::Production),
        Ok(value) => Err(format!("unknown H3 transport profile: {value}")),
        Err(error) => Err(error.to_string()),
    });
pub fn transport_profile() -> Result<TransportProfile> {
    PROFILE
        .as_ref()
        .copied()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

/// Matched transport settings for library/path attribution. This is an explicit
/// experimental profile, separate from the normal production comparison.
pub(crate) fn quinn_transport() -> Result<quinn::TransportConfig> {
    if transport_profile()? == TransportProfile::Production {
        return Ok(crate::quic_transport::tuned_transport_config(None));
    }
    let mut transport = quinn::TransportConfig::default();
    transport.send_window(256 * 1024);
    transport.receive_window(quinn::VarInt::from_u32(256 * 1024));
    transport.stream_receive_window(quinn::VarInt::from_u32(64 * 1024));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(8));
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(30)
            .try_into()
            .expect("valid QUIC timeout"),
    ));
    transport.initial_mtu(1350);
    transport.mtu_discovery_config(None);
    Ok(transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_selection_never_falls_back() {
        assert_eq!(parse("quinn-shared").unwrap(), H3Backend::QuinnShared);
        assert_eq!(parse("quinn-native").unwrap(), H3Backend::QuinnNative);
        assert!(parse("").is_err());
        assert!(parse("auto").is_err());
        assert_eq!(
            parse("tokio-quiche-native").is_ok(),
            cfg!(feature = "h3-quiche")
        );
    }
}
