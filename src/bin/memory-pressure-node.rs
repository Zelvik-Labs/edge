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
use zelvik_edge::cache_manager::CACHE;
use zelvik_edge::config_models::{HTTP3Policy, SSLCertConfig, ServerConfig};
use zelvik_edge::firewall::state::WafStateManager;
use zelvik_edge::firewall::verifier::WafVerifier;
use zelvik_edge::memory_governor::{MEMORY_GOVERNOR, MemoryPressureLevel};
use zelvik_edge::memory_reclaim::{last_reclaim_stats, reclaim_activity, request_reclaim};
use zelvik_edge::memory_shed::shed_stats;
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

async fn control(acceptance_enabled: bool, cache_enabled: bool) -> Result<()> {
    let cg = cgroup_path()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:19090").await?;
    let mut ballast: Vec<Box<[u8]>> = Vec::new();
    loop {
        let (socket, _) = listener.accept().await?;
        let mut stream = BufReader::new(socket);
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        let args: Vec<_> = line.split_whitespace().collect();
        let mut forced_reclaim = None;
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
            ["reclaim", level] => {
                let level = match *level {
                    "elevated" => MemoryPressureLevel::Elevated,
                    "high" => MemoryPressureLevel::High,
                    "critical" => MemoryPressureLevel::Critical,
                    _ => anyhow::bail!("invalid reclaim level"),
                };
                forced_reclaim = Some(request_reclaim(level).is_some());
            }
            _ => anyhow::bail!("invalid test command"),
        }
        // The acceptance sources need the production cache's own accounting:
        // weight-based owned bytes after real requests, not a synthetic mark.
        // Only acceptance runs touch the cache or add fields after the
        // allocation snapshot, so benchmark replies stay byte-for-byte equal.
        let cache = if acceptance_enabled && cache_enabled {
            let stats = CACHE.storage.runtime_stats().await;
            MEMORY_GOVERNOR.set_cache_l1_bytes(stats.memory_bytes);
            Some(stats)
        } else {
            None
        };
        let s = MEMORY_GOVERNOR.snapshot(2);
        let mut reply = json!({
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
        if acceptance_enabled {
            let reclaim = last_reclaim_stats();
            let activity = reclaim_activity();
            let shed = shed_stats();
            let object = reply.as_object_mut().context("reply is not an object")?;
            object.insert("forcedReclaim".into(), json!(forced_reclaim));
            // The l4 pressure level de-escalates on a 5s dwell and gates the
            // per-IP TCP active limit, so acceptance must not mistake its
            // still-tightened window for a memory-admission refusal.
            object.insert(
                "l4Pressure".into(),
                json!(zelvik_edge::l4_defense::current_pressure_level().as_str()),
            );
            object.insert(
                "tcpActivePerIpLimit".into(),
                json!(zelvik_edge::l4_defense::current_tcp_active_limit_per_ip()),
            );
            object.insert(
                "cache".into(),
                cache
                    .map(|cache| {
                        json!({
                            "policy": cache.policy_type,
                            "memoryCount": cache.memory_count,
                            "memoryBytes": cache.memory_bytes,
                            "budgetBytes": cache.memory_budget_bytes,
                            "diskCount": cache.disk_count,
                            "diskBytes": cache.disk_bytes,
                            "bloomEstimatedBytes": cache.bloom_estimated_bytes,
                            "negativeCacheCount": cache.negative_cache_count
                        })
                    })
                    .unwrap_or(serde_json::Value::Null),
            );
            object.insert("reclaim".into(), json!({
                "runs_total": activity.runs_total,
                "bytes_freed_estimate": activity.bytes_freed_estimate,
                "last_duration_ms": activity.last_duration_ms,
                "panics_total": activity.panics_total,
                "last": reclaim.map(|r| json!({
                    "l1_entries_removed": r.l1_entries_removed,
                    "l1_bytes_freed_estimate": r.l1_bytes_freed_estimate,
                    "bloom_layers_removed": r.bloom_layers_removed,
                    "negative_cache_entries_removed": r.negative_cache_entries_removed,
                    "geo_cache_entries_removed": r.geo_cache_entries_removed,
                    "ua_cache_entries_removed": r.ua_cache_entries_removed,
                    "tls_connectors_removed": r.tls_connectors_removed,
                    "waf_regex_entries_removed": r.waf_regex_entries_removed,
                    "surrogate_index_tags_removed": r.surrogate_index_tags_removed,
                    "process_rss_before_bytes": r.process_rss_before_bytes,
                    "process_rss_after_bytes": r.process_rss_after_bytes
                }))
            }));
            object.insert("shed".into(), json!({
                "keepalive_shed_active": shed.keepalive_shed_active,
                "critical_samples": shed.critical_samples,
                "drained_connections_total": shed.drained_connections_total,
                "keepalive_marked_total": shed.keepalive_marked_total,
                "reclaim_exhausted": shed.reclaim_exhausted,
                "final_resort_passes_total": shed.final_resort_passes_total,
                "last_final_resort_at_ms": shed.last_final_resort_at_ms,
                "drain_reason": "MemoryPressureShed"
            }));
            object.insert("governor".into(), json!({
                "cgroup_managed": s.cgroup_managed,
                "cgroup_max_bytes": s.cgroup_memory_max_bytes,
                "cgroup_high_bytes": s.cgroup_memory_high_bytes,
                "cgroup_reclaimable_bytes": s.cgroup_reclaimable_bytes,
                "cache_budget_bytes": s.cache_budget_bytes,
                "cache_l1_bytes": s.cache_l1_bytes,
                "process_anon_rss_bytes": s.process_anon_rss_bytes,
                "process_file_rss_bytes": s.process_file_rss_bytes,
                "unaccounted_anon_rss_bytes": s.unaccounted_anon_rss_bytes,
                "psi_some_avg10_x100": s.psi_some_avg10_x100,
                "psi_full_avg10_x100": s.psi_full_avg10_x100,
                "resident_cache_metadata_bytes": s.resident_memory.cache_metadata_used_bytes,
                "request_workspace_used_bytes": s.request_workspace_used_bytes,
                "memory_available_bytes": s.memory_available_bytes,
                "account_headroom_bytes": s.account_headroom_bytes,
                "account_grantable_bytes": s.account_grantable_bytes,
                "account_pending_bytes": s.account_pending_bytes,
                "account_committed_unconfirmed_bytes": s.account_committed_unconfirmed_bytes,
                "account_committed_live_bytes": s.account_committed_live_bytes,
                "connection_admission_used_bytes": s.connection_admission_used_bytes,
                "connection_budget_bytes": s.connection_budget_bytes,
                "estimated_http_connections": s.estimated_http_connections,
                "estimated_h2_streams": s.estimated_h2_streams,
                "estimated_h3_requests": s.estimated_h3_requests,
                "estimated_origin_connects": s.estimated_origin_connects
            }));
        }
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
    let acceptance_enabled = matches!(std::env::var("PRESSURE_ACCEPTANCE").as_deref(), Ok("1"));
    let cache_enabled = matches!(std::env::var("PRESSURE_CACHE").as_deref(), Ok("1"));
    if cache_enabled {
        // Cache fills are discarded without the metadata writer: it owns the
        // Mace publish and in-memory index that make an object a real L1
        // entry. Start the same production task the node starts.
        zelvik_edge::metrics::storage::start_cache_access_flusher();
    }
    // Acceptance source "cache" fills the real production TinyUFO L1 through
    // ordinary GET responses. Benchmark and ballast-only runs keep the
    // historical fixture configuration byte-for-byte.
    let web = if cache_enabled {
        json!({
            "isOn": true,
            "cache": {
                "isOn": true,
                "cacheRefs": [{
                    "isOn": true,
                    "life": {"count": 3600, "unit": "s"},
                    "methods": ["GET"],
                    "status": [200],
                    "maxSize": {"count": 8, "unit": "m"},
                    "allowChunkedEncoding": false,
                    "onlyURLPatterns": [{"type": "prefix", "pattern": "/cache/"}]
                }]
            }
        })
    } else {
        json!({"isOn": true})
    };
    let server: ServerConfig = serde_json::from_value(json!({
        "id": 1, "userId": 1, "isOn": true,
        "serverNames": [{"name": "localhost"}, {"name": "127.0.0.1"}],
        "http": {"isOn": true, "listen": [{"host": "127.0.0.1", "portRange": "19080"}]},
        "https": {"isOn": true, "http3Enabled": true,
            "listen": [{"host": "127.0.0.1", "portRange": "19443"}]},
        "web": web,
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
    ))
    .expect("fixture configuration installs");
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
    rt.block_on(control(acceptance_enabled, cache_enabled))
}
