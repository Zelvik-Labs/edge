//! Regression test for defect #2: `SSLPolicyConfig.http2Enabled` / `minVersion`
//! were not consumed by the HTTP/1.1+2 TLS listener — the listener hardcoded
//! ALPN `[h2, http/1.1]` and TLS 1.2+1.3 regardless of policy.
//!
//! These tests build the listener config through the production path
//! (`zelvik_edge::ssl::build_rustls_server_config_for_policy`, fed by
//! `TlsPolicy::from_ssl_policy` from the parsed `sslPolicy`) and prove the
//! behavior with real in-memory rustls handshakes — not by reading config
//! fields alone.
//!
//! Covered:
//!   * `http2Enabled=false` + `minVersion=TLSv1.3`
//!       - a TLS 1.2-only client must fail to handshake;
//!       - an ALPN-`h2`-only client must not negotiate `h2`.
//!   * `http2Enabled=true` + `minVersion=TLSv1.2` (control)
//!       - a TLS 1.2 client must handshake and negotiate `h2`.
//!   * the string→floor mapping (TLS 1.0/1.1 rejected) and the per-port union
//!     used when several servers share one acceptor.

use std::sync::Arc;
use zelvik_edge::config_models::{HTTPSConfig, SSLCertConfig};
use zelvik_edge::ssl::{
    DynamicCertSelector, TlsMinVersion, TlsPolicy, build_rustls_server_config_for_policy,
    sync_certs,
};

const CERT_PEM: &str = include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem");
const KEY_PEM: &str = include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem");

/// Accepts any server certificate — used only to observe ALPN/version negotiation.
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            ED25519,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
        ]
    }
}

/// Runs a real in-memory TLS handshake and returns the negotiated protocol
/// version and ALPN protocol as observed by the client. Returns `Err` when the
/// handshake does not complete (e.g. the server rejects the client's version or
/// ALPN offer).
fn try_handshake(
    server_config: Arc<rustls::ServerConfig>,
    client_versions: &[&'static rustls::SupportedProtocolVersion],
    client_alpn: Vec<Vec<u8>>,
) -> anyhow::Result<(Option<rustls::ProtocolVersion>, Option<Vec<u8>>)> {
    let provider = Arc::new(zelvik_edge::tls_crypto::default_crypto_provider());
    let mut client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(client_versions)
        .expect("client protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    client_config.alpn_protocols = client_alpn;

    let mut server = rustls::ServerConnection::new(server_config)?;
    let mut client = rustls::ClientConnection::new(
        Arc::new(client_config),
        rustls::pki_types::ServerName::try_from("openrusty.org").unwrap(),
    )?;

    for _ in 0..32 {
        if client.wants_write() {
            let mut buf = Vec::new();
            client.write_tls(&mut buf).unwrap();
            let mut rd = &buf[..];
            server.read_tls(&mut rd)?;
            server.process_new_packets()?;
        }
        if server.wants_write() {
            let mut buf = Vec::new();
            server.write_tls(&mut buf).unwrap();
            let mut rd = &buf[..];
            client.read_tls(&mut rd)?;
            client.process_new_packets()?;
        }
        if !client.is_handshaking() && !server.is_handshaking() {
            break;
        }
    }
    if client.is_handshaking() || server.is_handshaking() {
        anyhow::bail!("TLS handshake did not complete");
    }
    Ok((
        client.protocol_version(),
        client.alpn_protocol().map(|p| p.to_vec()),
    ))
}

fn alpn_of(server_config: &rustls::ServerConfig) -> Vec<String> {
    server_config
        .alpn_protocols
        .iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// Parses the exact `https.sslPolicy` shape the admin console pushes, syncs the
/// cert, and builds the listener config through the production path.
async fn listener_config(
    http2_enabled: bool,
    min_version: &str,
) -> (rustls::ServerConfig, TlsPolicy) {
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id": 7001,
        "isOn": true,
        "isDefault": true,
        "certData": CERT_PEM,
        "keyData": KEY_PEM,
        "dnsNames": ["openrusty.org"]
    }))
    .expect("cert config should parse");

    let https: HTTPSConfig = serde_json::from_value(serde_json::json!({
        "isOn": true,
        "listen": [{"protocol": "https", "portRange": "443"}],
        "sslPolicy": {
            "id": 42,
            "isOn": true,
            "http2Enabled": http2_enabled,
            "minVersion": min_version,
            "certs": [cert.clone()]
        }
    }))
    .expect("https config should parse");

    let policy = https.ssl_policy.as_ref().expect("sslPolicy present");
    let tls_policy = TlsPolicy::from_ssl_policy(policy);

    let selector = DynamicCertSelector::new();
    sync_certs(&selector, std::slice::from_ref(&cert)).await;
    assert!(selector.has_default_cert());

    let config = build_rustls_server_config_for_policy(Arc::new(selector), tls_policy)
        .expect("listener config builds");
    (config, tls_policy)
}

#[tokio::test]
async fn http2_off_and_min_tls_1_3_are_enforced_by_the_listener() {
    let (config, policy) = listener_config(false, "TLSv1.3").await;
    assert!(!policy.http2_enabled, "policy should disable HTTP/2");
    assert_eq!(policy.min_version, TlsMinVersion::Tls13);

    // ALPN must not advertise h2 when HTTP/2 is disabled.
    assert_eq!(
        alpn_of(&config),
        vec!["http/1.1".to_string()],
        "listener must not advertise h2 when http2Enabled=false"
    );

    let config = Arc::new(config);

    // (1) A TLS 1.2-only client must fail: minVersion=TLSv1.3 leaves no common
    //     protocol version.
    let tls12 = try_handshake(
        Arc::clone(&config),
        &[&rustls::version::TLS12],
        vec![b"h2".to_vec(), b"http/1.1".to_vec()],
    );
    assert!(
        tls12.is_err(),
        "TLS 1.2-only client must fail against a TLS 1.3-only listener, got {:?}",
        tls12.map(|(v, a)| (v, a.map(|p| String::from_utf8_lossy(&p).into_owned())))
    );

    // (2) A client that offers only ALPN `h2` must never negotiate h2 (the
    //     handshake fails on the ALPN mismatch, or at minimum h2 is not chosen).
    let h2_only = try_handshake(
        Arc::clone(&config),
        &[&rustls::version::TLS13],
        vec![b"h2".to_vec()],
    );
    if let Ok((_, alpn)) = h2_only {
        assert_ne!(
            alpn.as_deref(),
            Some(&b"h2"[..]),
            "h2 must not be negotiated when http2Enabled=false"
        );
    }

    // (3) Sanity: a TLS 1.3 client offering http/1.1 still connects, proving the
    //     listener is healthy and only h2/TLS 1.2 are refused.
    let (version, alpn) = try_handshake(
        Arc::clone(&config),
        &[&rustls::version::TLS13],
        vec![b"http/1.1".to_vec()],
    )
    .expect("TLS 1.3 + http/1.1 client should handshake");
    assert_eq!(version, Some(rustls::ProtocolVersion::TLSv1_3));
    assert_eq!(alpn.as_deref(), Some(&b"http/1.1"[..]));
}

#[tokio::test]
async fn http2_on_and_min_tls_1_2_negotiate_h2_over_tls_1_2() {
    let (config, policy) = listener_config(true, "TLSv1.2").await;
    assert!(policy.http2_enabled);
    assert_eq!(policy.min_version, TlsMinVersion::Tls12);

    assert_eq!(
        alpn_of(&config),
        vec!["h2".to_string(), "http/1.1".to_string()],
        "listener must advertise h2 when http2Enabled=true"
    );

    let (version, alpn) = try_handshake(
        Arc::new(config),
        &[&rustls::version::TLS12],
        vec![b"h2".to_vec(), b"http/1.1".to_vec()],
    )
    .expect("TLS 1.2 client should handshake with a TLS 1.2+1.3 listener");
    assert_eq!(version, Some(rustls::ProtocolVersion::TLSv1_2));
    assert_eq!(
        alpn.as_deref(),
        Some(&b"h2"[..]),
        "h2 must be negotiated when http2Enabled=true"
    );
}

#[test]
fn min_version_strings_resolve_to_supported_floors() {
    assert_eq!(TlsMinVersion::from_config_str("TLSv1.3"), TlsMinVersion::Tls13);
    assert_eq!(TlsMinVersion::from_config_str("TLSv1.2"), TlsMinVersion::Tls12);
    // Unset defaults to the TLS 1.2 floor.
    assert_eq!(TlsMinVersion::from_config_str(""), TlsMinVersion::Tls12);
    assert_eq!(TlsMinVersion::from_config_str("   "), TlsMinVersion::Tls12);
    // rustls does not support TLS 1.0/1.1: rejected and folded to the TLS 1.2 floor.
    assert_eq!(TlsMinVersion::from_config_str("TLSv1.1"), TlsMinVersion::Tls12);
    assert_eq!(TlsMinVersion::from_config_str("TLSv1.0"), TlsMinVersion::Tls12);
    // Unknown values are not silently treated as the default.
    assert_eq!(TlsMinVersion::from_config_str("TLSv2.0"), TlsMinVersion::Tls12);
}

#[test]
fn tls_policy_union_is_http2_or_and_most_permissive_floor() {
    let strict = TlsPolicy {
        http2_enabled: false,
        min_version: TlsMinVersion::Tls13,
    };
    let loose = TlsPolicy {
        http2_enabled: true,
        min_version: TlsMinVersion::Tls12,
    };

    let merged = strict.union(loose);
    assert!(merged.http2_enabled, "HTTP/2 is enabled if either side enables it");
    assert_eq!(
        merged.min_version,
        TlsMinVersion::Tls12,
        "the shared acceptor takes the most permissive floor"
    );

    // TLS 1.3 survives only when every contributor requires it.
    assert_eq!(strict.union(strict).min_version, TlsMinVersion::Tls13);
    assert_eq!(loose.union(strict).min_version, TlsMinVersion::Tls12);
    // 缺省是「开」：站点没有下发 sslPolicy 时不能静默失去 HTTP/2。
    assert_eq!(TlsPolicy::default(), TlsPolicy {
        http2_enabled: true,
        min_version: TlsMinVersion::Tls12,
    });
}

/// 字段缺失必须等价于开启：`http2Enabled` 在配置里不出现时，监听器仍要通告
/// `h2`（修复前边缘恒通告 h2；控制面与面板的默认也都是开）。只有显式 false
/// 才关闭。这条锁死「把 unset 当成关」的回归。
#[tokio::test]
async fn missing_http2_enabled_field_keeps_http2_on() {
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id": 7002,
        "isOn": true,
        "isDefault": true,
        "certData": CERT_PEM,
        "keyData": KEY_PEM,
        "dnsNames": ["openrusty.org"]
    }))
    .expect("cert config should parse");

    let https: HTTPSConfig = serde_json::from_value(serde_json::json!({
        "isOn": true,
        "listen": [{"protocol": "https", "portRange": "443"}],
        "sslPolicy": {
            "id": 43,
            "isOn": true,
            "certs": [cert.clone()]
        }
    }))
    .expect("https config without http2Enabled should parse");

    let policy = https.ssl_policy.as_ref().expect("sslPolicy present");
    assert!(
        policy.http2_enabled,
        "an absent http2Enabled field must deserialize as enabled"
    );

    let tls_policy = TlsPolicy::from_ssl_policy(policy);
    let selector = DynamicCertSelector::new();
    sync_certs(&selector, std::slice::from_ref(&cert)).await;
    let config = build_rustls_server_config_for_policy(Arc::new(selector), tls_policy)
        .expect("listener config builds");

    assert!(
        alpn_of(&config).contains(&"h2".to_string()),
        "h2 must still be advertised when the field is absent, got {:?}",
        alpn_of(&config)
    );

    // 显式 false 仍然关闭——缺省为开不能变成「关不掉」。
    let disabled: HTTPSConfig = serde_json::from_value(serde_json::json!({
        "isOn": true,
        "listen": [{"protocol": "https", "portRange": "443"}],
        "sslPolicy": { "id": 44, "isOn": true, "http2Enabled": false, "certs": [cert] }
    }))
    .expect("https config should parse");
    let disabled_policy = disabled.ssl_policy.as_ref().expect("sslPolicy present");
    assert!(
        !disabled_policy.http2_enabled,
        "an explicit false must still disable HTTP/2"
    );
}
