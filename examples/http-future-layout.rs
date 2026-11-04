//! Inspect real callback frame layouts without polling request logic.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(unix)]
#[path = "../benches/support/http_callback_construct.rs"]
mod construction;

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use pingora_proxy::{ProxyHttp, Session};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use zelvik_edge::api_config::ApiConfig;
    use zelvik_edge::config::ConfigStore;
    use zelvik_edge::firewall::state::WafStateManager;
    use zelvik_edge::firewall::verifier::WafVerifier;
    use zelvik_edge::proxy::EdgeProxy;
    use zelvik_edge::ssl::DynamicCertSelector;

    let api_config = Arc::new(ApiConfig {
        rpc_tls: Default::default(),
        rpc_endpoints: Vec::new(),
        rpc_disable_update: true,
        instance_id: "nd_0123456789".to_string(),
        secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".to_string(),
        comm_mode: String::new(),
        gcloud_node_id: "1".to_string(),
        access_log_pipeline: Default::default(),
        relay: Default::default(),
        kernel_tuning: Default::default(),
        cache: Default::default(),
    });
    let challenge_key = api_config.challenge_key()?;
    let proxy = EdgeProxy {
        config: Arc::new(ConfigStore::new()),
        waf_state: Arc::new(WafStateManager::new()),
        api_config,
        challenge_key,
        cert_selector: Arc::new(DynamicCertSelector::new()),
        waf_verifier: Arc::new(WafVerifier::new(&challenge_key)),
        tls_downstream: false,
    };
    let (mut client, downstream) = tokio::net::UnixStream::pair()?;
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .await?;
    let mut session = Session::new_h1(Box::new(pingora_core::protocols::l4::stream::Stream::from(
        downstream,
    )));
    session
        .downstream_session
        .read_request()
        .await
        .expect("fixed request fixture must parse");
    let mut context = proxy.new_ctx();
    let mut response = pingora::http::ResponseHeader::build(200, Some(4))
        .expect("fixed response fixture must be valid");
    #[cfg(feature = "http-layout-probe")]
    let phases = proxy.inspect_unpolled_phase_layouts(&mut session, &mut context);
    #[cfg(not(feature = "http-layout-probe"))]
    let phases = serde_json::Value::Null;
    let mut rows = serde_json::Map::new();
    macro_rules! frame {
        ($name:literal, $future:expr) => {{
            let future = $future;
            // The dynamic future vtable reports its concrete frame, unlike
            // size_of_val(&future), which would only measure the box pointer.
            let value = future.as_ref().get_ref();
            let frame_bytes = std::mem::size_of_val(value);
            let alignment = std::mem::align_of_val(value);
            drop(future);
            let blocks = construction::blocks(|| {
                let future = $future;
                std::hint::black_box(future.as_ref().get_ref());
                drop(future);
            });
            rows.insert(
                $name.to_string(),
                serde_json::json!({
                    "frame_bytes": frame_bytes,
                    "alignment": alignment,
                    "construction_drop_blocks": blocks,
                }),
            );
        }};
    }
    frame!(
        "early_request_filter",
        proxy.early_request_filter(&mut session, &mut context)
    );
    frame!(
        "request_filter",
        proxy.request_filter(&mut session, &mut context)
    );
    frame!(
        "upstream_peer",
        proxy.upstream_peer(&mut session, &mut context)
    );
    frame!(
        "upstream_response_filter",
        proxy.upstream_response_filter(&mut session, &mut response, &mut context)
    );
    frame!(
        "response_filter",
        proxy.response_filter(&mut session, &mut response, &mut context)
    );
    frame!("logging", proxy.logging(&mut session, None, &mut context));
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scope": "unpolled real callback layouts and construction/drop; not request execution, allocation counts or throughput",
            "allocator": "mimalloc, as in the production executable",
            "callbacks": rows,
            "request_phases": phases,
        }))?
    );
    Ok(())
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("the callback layout fixture requires UnixStream::pair")
}
