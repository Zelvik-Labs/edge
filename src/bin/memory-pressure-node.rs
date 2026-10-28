//! Isolated loopback EdgeProxy fixture for real cgroup pressure and paired benchmarks.
//! The control port deliberately allocates touched ballast to a requested cgroup
//! occupancy. It belongs only in a disposable test cgroup, never a serving node.
use anyhow::{Context, Result};
use pingora_core::server::configuration::ServerConf;
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use zelvik_edge::api_config::ApiConfig;
use zelvik_edge::config::ConfigStore;
use zelvik_edge::config_models::{HTTP3Policy, SSLCertConfig, ServerConfig};
use zelvik_edge::firewall::state::WafStateManager;
use zelvik_edge::firewall::verifier::WafVerifier;
use zelvik_edge::memory_governor::MEMORY_GOVERNOR;
use zelvik_edge::proxy::EdgeProxy;
use zelvik_edge::ssl::DynamicCertSelector;

struct CountedAllocator;
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
#[global_allocator]
static ALLOCATOR: CountedAllocator = CountedAllocator;

unsafe impl GlobalAlloc for CountedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { mimalloc::MiMalloc.alloc(layout) };
        if !ptr.is_null() && COUNT.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { mimalloc::MiMalloc.realloc(ptr, layout, size) };
        if !result.is_null() && COUNT.load(Ordering::Relaxed) {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        }
        result
    }
}

fn cgroup_path() -> Result<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup")?;
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("cgroup v2 required")?;
    Ok(std::path::Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

async fn control() -> Result<()> {
    let cg = cgroup_path()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:19090").await?;
    let mut ballast: Vec<Box<[u8]>> = Vec::new();
    loop {
        let (socket, _) = listener.accept().await?;
        let mut stream = BufReader::new(socket);
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        let args: Vec<_> = line.split_whitespace().collect();
        match args.as_slice() {
            ["fill", pct] => {
                let pct: u64 = pct.parse()?;
                anyhow::ensure!(pct <= 98, "test occupancy must be <=98%");
                let max: u64 = std::fs::read_to_string(cg.join("memory.max"))?
                    .trim()
                    .parse()?;
                let target = max * pct / 100;
                loop {
                    let current: u64 = std::fs::read_to_string(cg.join("memory.current"))?
                        .trim()
                        .parse()?;
                    if current.saturating_add(1 << 20) > target {
                        break;
                    }
                    // Touch every page; reserve/virtual allocation alone is not pressure.
                    ballast.push(vec![0x5a; 1 << 20].into_boxed_slice());
                    tokio::task::yield_now().await;
                }
            }
            ["release"] => {
                ballast.clear();
            }
            ["count", flag] => {
                COUNT.store(*flag == "on", Ordering::Relaxed);
            }
            ["stats"] => {}
            _ => anyhow::bail!("invalid test command"),
        }
        let s = MEMORY_GOVERNOR.snapshot(2);
        let reply = json!({
            "pid": std::process::id(), "cgroup": cg, "ballastBytes": ballast.len() * (1 << 20),
            "pressure": s.memory_pressure_level.as_str(), "rss": s.process_rss_bytes,
            "pss": s.process_pss_bytes, "h2": s.estimated_h2_streams,
            "h3": s.estimated_h3_requests, "httpConnections": s.estimated_http_connections,
            "h2PerConnection": s.h2_stream_limit_per_connection,
            "h3PerConnection": s.h3_request_limit_per_connection,
            "h2Rejects": s.admission_rejects.h2_stream, "h3Rejects": s.admission_rejects.h3_request,
            "originRejects": s.admission_rejects.origin_connect,
            "grantable": s.account_grantable_bytes,
            "allocations": ALLOCS.load(Ordering::Relaxed),
            "allocatedBytes": ALLOC_BYTES.load(Ordering::Relaxed),
            "reallocations": REALLOCS.load(Ordering::Relaxed)
        });
        stream
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await?;
    }
}

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let _entered = rt.enter();
    zelvik_edge::utils::time::init_local_timezone();
    zelvik_edge::memory_reclaim::start_reclaim_monitor();
    zelvik_edge::metrics::init_http_dimension_worker(1024);
    zelvik_edge::metrics::start_pressure_updater();
    zelvik_edge::proxy::start_request_limit_cleanup_task();
    let store = Arc::new(ConfigStore::new());
    let waf = Arc::new(WafStateManager::new());
    let api: ApiConfig = serde_json::from_value(json!({
        "rpc.endpoints": ["http://127.0.0.1:1"], "rpc.disableUpdate": true,
        "instanceId": "pressure-fixture", "secret": "0000000000000000000000000000000000000000000000000000000000000000"
    }))?;
    let api = Arc::new(api);
    let cert = Arc::new(DynamicCertSelector::new());
    let tls_dir = std::env::var("PRESSURE_TLS_DIR").context("PRESSURE_TLS_DIR required")?;
    let certificates: Vec<SSLCertConfig> = serde_json::from_value(json!([{
        "id": 1, "isOn": true, "isDefault": true,
        "certData": std::fs::read_to_string(format!("{tls_dir}/cert.pem"))?,
        "keyData": std::fs::read_to_string(format!("{tls_dir}/key.pem"))?,
        "dnsNames": ["localhost", "127.0.0.1"]
    }]))?;
    rt.block_on(zelvik_edge::ssl::sync_certs(&cert, &certificates));
    let server: ServerConfig = serde_json::from_value(json!({
        "id": 1, "userId": 1, "isOn": true,
        "serverNames": [{"name": "localhost"}, {"name": "127.0.0.1"}],
        "http": {"isOn": true, "listen": [{"host": "127.0.0.1", "portRange": "19080"}]},
        "https": {"isOn": true, "http3Enabled": true,
            "listen": [{"host": "127.0.0.1", "portRange": "19443"}]},
        "web": {"isOn": true},
        "reverseProxy": {"isOn": true, "primaryOrigins": [{
            "id": 1, "isOn": true, "addr": "http://127.0.0.1:19081", "weight": 1,
            "followHost": true
        }], "scheduling": {"code": "roundRobin"}}
    }))?;
    let lb = zelvik_edge::lb_factory::build_lb(
        1,
        server.reverse_proxy.as_ref().unwrap(),
        1,
        &HashMap::new(),
        false,
        true,
    )
    .0;
    let server = Arc::new(server);
    let servers = server
        .get_plain_server_names()
        .into_iter()
        .map(|n| (n, server.clone()))
        .collect();
    let routes = server
        .get_plain_server_names()
        .into_iter()
        .map(|n| (n, lb.clone()))
        .collect();
    let h3: HTTP3Policy = serde_json::from_value(json!({"isOn":true,"port":19443}))?;
    rt.block_on(store.update_config(
        1,
        1,
        0,
        1,
        vec![server],
        servers,
        routes,
        HashMap::from([(1, lb)]),
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        0,
        1,
        true,
        false,
        HashMap::new(),
        false,
        false,
        "random".into(),
        HashMap::new(),
        None,
        true,
        false,
        "pressure-fixture".into(),
        false,
        false,
        0,
        true,
        false,
        false,
        String::new(),
        None,
        None,
        vec![],
        vec![],
        vec![],
        HashMap::new(),
        HashMap::new(),
        HashMap::from([(1, h3)]),
        HashMap::new(),
        HashMap::new(),
        None,
        None,
    ));
    let key = api.challenge_key().context("fixture challenge key")?;
    let proxy = EdgeProxy {
        config: store.clone(),
        waf_state: waf,
        api_config: api,
        challenge_key: key,
        cert_selector: cert.clone(),
        waf_verifier: Arc::new(WafVerifier::new(&key)),
        tls_downstream: false,
    };
    let conf = Arc::new(ServerConf {
        threads: 2,
        ..Default::default()
    });
    rt.spawn(
        zelvik_edge::http_proxy_manager::HttpProxyManager::new(
            (*store).clone(),
            cert.clone(),
            proxy.clone(),
            conf.clone(),
        )
        .start_listeners(),
    );
    rt.spawn(
        zelvik_edge::http3_proxy_manager::Http3ProxyManager::new(
            (*store).clone(),
            cert,
            proxy,
            conf,
        )
        .start_listeners(),
    );
    rt.block_on(control())
}
