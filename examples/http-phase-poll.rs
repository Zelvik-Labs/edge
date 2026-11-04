//! Controlled ready-GET costs of actual methods; no capacity or pure-CPU claim.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use pingora_core::protocols::{GetSocketDigest, SocketDigest};
    use pingora_proxy::{ProxyHttp, Session};
    use std::collections::HashMap;
    use std::hint::black_box;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;
    use zelvik_edge::api_config::ApiConfig;
    use zelvik_edge::config::ConfigStore;
    use zelvik_edge::config_models::ServerConfig;
    use zelvik_edge::firewall::state::WafStateManager;
    use zelvik_edge::firewall::verifier::WafVerifier;
    use zelvik_edge::proxy::{EdgeProxy, RequestPhase};
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
    let config = Arc::new(ConfigStore::new());
    let server: ServerConfig = serde_json::from_value(serde_json::json!({
        "id":1,"userId":1,"isOn":true,"serverNames":[{"name":"example.test"}],
        "web":{"isOn":true},
        "reverseProxy":{"isOn":true,"primaryOrigins":[{"id":1,"isOn":true,
            "addr":"http://127.0.0.1:8081","weight":100}],
            "scheduling":{"code":"polling","options":{}}}
    }))?;
    let (lb, _) = zelvik_edge::lb_factory::build_lb(
        1,
        server.reverse_proxy.as_ref().context("fixture origin")?,
        1,
        &HashMap::new(),
        false,
        true,
    );
    let server = Arc::new(server);
    config
        .replace_all_servers(
            vec![server.clone()],
            HashMap::from([("example.test".to_string(), server)]),
            HashMap::from([("example.test".to_string(), lb.clone())]),
            HashMap::from([(1, lb)]),
        )
        .await?;
    let proxy = EdgeProxy {
        config,
        waf_state: Arc::new(WafStateManager::new()),
        api_config,
        challenge_key,
        cert_selector: Arc::new(DynamicCertSelector::new()),
        waf_verifier: Arc::new(WafVerifier::new(&challenge_key)),
        tls_downstream: false,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let (client, downstream) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr()?),
        listener.accept()
    );
    let mut client = client?;
    let downstream = downstream?.0;
    client.write_all(b"GET /off/payload HTTP/1.1\r\nHost: example.test\r\nUser-Agent: edge-phase-poll/1\r\n\r\n").await?;
    let mut stream = pingora_core::protocols::l4::stream::Stream::from(downstream);
    stream.set_socket_digest(SocketDigest::from_raw_fd(stream.as_raw_fd()));
    let mut session = Session::new_h1(Box::new(stream));
    ensure!(
        session.downstream_session.read_request().await?,
        "fixture request must parse"
    );
    ensure!(
        session.client_addr().is_some(),
        "fixture must have a real TCP digest"
    );
    zelvik_edge::proxy::inspect_with_http1_parse_tracking(async {
        for _ in 0..1000 {
            let mut context = proxy.new_ctx();
            ensure!(!proxy.request_filter(&mut session, &mut context).await?);
            ensure!(context.request_phase == RequestPhase::Proxy && context.lb.is_some());
        }
        let scenarios = [
            "new-context",
            "request-filter",
            "phase-polls",
            "clock-calibration",
        ];
        for round in 0..8 {
            for offset in 0..4 {
                let scenario = scenarios[(round + offset) % scenarios.len()];
                for block in 0..20 {
                    let iterations = 20_000_u64;
                    let mut phases = [[0_u64; 3]; 10];
                    let mut calibration_ns = 0_u64;
                    let start = Instant::now();
                    for _ in 0..iterations {
                        match scenario {
                            "new-context" => {
                                black_box(proxy.new_ctx());
                            }
                            "request-filter" => {
                                let mut context = proxy.new_ctx();
                                ensure!(!proxy.request_filter(&mut session, &mut context).await?);
                                ensure!(
                                    context.request_phase == RequestPhase::Proxy
                                        && context.lb.is_some()
                                );
                                black_box(&context);
                            }
                            "phase-polls" => {
                                let mut context = proxy.new_ctx();
                                let costs = proxy
                                    .inspect_request_phase_polls(&mut session, &mut context)
                                    .await?;
                                ensure!(
                                    context.request_phase == RequestPhase::Proxy
                                        && context.lb.is_some()
                                );
                                for (total, cost) in phases.iter_mut().zip(costs) {
                                    for (value, addition) in total.iter_mut().zip(cost) {
                                        *value += addition;
                                    }
                                }
                                black_box(&context);
                            }
                            "clock-calibration" => {
                                let mut ready = std::pin::pin!(std::future::ready(black_box(())));
                                std::future::poll_fn(|cx| {
                                    let stamp = Instant::now();
                                    let polled = std::future::Future::poll(ready.as_mut(), cx);
                                    calibration_ns += stamp.elapsed().as_nanos() as u64;
                                    polled
                                })
                                .await;
                            }
                            _ => unreachable!(),
                        }
                    }
                    let elapsed_ns = start.elapsed().as_nanos();
                    if scenario == "phase-polls" {
                        ensure!(
                            phases.iter().all(|row| row[1] == iterations && row[2] == 0),
                            "ready-GET phase measurement cannot include pending polls"
                        );
                    }
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "round":round,"scenario":scenario,"block":block,"iterations":iterations,
                            "elapsed_ns":elapsed_ns,"phases_active_ns_polls_pending":phases,
                            "calibration_active_ns":calibration_ns,
                        }))?
                    );
                    std::io::stdout().flush()?;
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("the controlled phase fixture requires a Unix TCP stream")
}
