use arc_swap::ArcSwapOption;
use lru::LruCache;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::num::NonZeroUsize;
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use woothee::parser::Parser;
use zelvik_geo_runtime::{GeoStore, GeoView};

pub struct GeoInfo {
    pub country: Arc<str>,
    pub country_id: i64,
    pub country_iso: Arc<str>,
    pub region: Arc<str>,
    pub region_id: i64,
    pub region_iso: Arc<str>,
    pub city: Arc<str>,
    pub city_id: i64,
    pub provider: Arc<str>,
}

impl Clone for GeoInfo {
    fn clone(&self) -> Self {
        Self {
            country: self.country.clone(),
            country_id: self.country_id,
            country_iso: self.country_iso.clone(),
            region: self.region.clone(),
            region_id: self.region_id,
            region_iso: self.region_iso.clone(),
            city: self.city.clone(),
            city_id: self.city_id,
            provider: self.provider.clone(),
        }
    }
}

pub struct RequestStats {
    pub geo: Option<GeoInfo>,
    pub browser: Arc<str>,
    pub browser_version: Arc<str>,
    pub os: Arc<str>,
    pub os_version: Arc<str>,
}

impl Clone for RequestStats {
    fn clone(&self) -> Self {
        Self {
            geo: self.geo.clone(),
            browser: self.browser.clone(),
            browser_version: self.browser_version.clone(),
            os: self.os.clone(),
            os_version: self.os_version.clone(),
        }
    }
}

static GEO_STORE: Lazy<ArcSwapOption<GeoStore>> = Lazy::new(ArcSwapOption::empty);
static GEO_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static GEO_CITY_BYTES: AtomicU64 = AtomicU64::new(0);
static GEO_CITY_GENERATION: AtomicU64 = AtomicU64::new(0);
static GEO_ASN_BYTES: AtomicU64 = AtomicU64::new(0);
static GEO_ASN_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Installs the one geo store the query path reads. The control-side agent and
/// the request path share this `Arc`, so an activation is visible to lookups
/// without a second mmap or a path-based reload.
///
/// Cached lookups from the previous generation are dropped here: the bundle
/// identity is part of a geo answer, and serving a cached country for an IP
/// after the data changed would silently mix generations. In-flight lookups
/// keep the snapshot they already loaded (ArcSwap), so a clear cannot tear one.
pub fn install_geo_store(
    store: Arc<GeoStore>,
    city_bytes: u64,
    asn_bytes: u64,
    asn_loaded: bool,
) {
    let generation = store.generation();
    GEO_CITY_BYTES.store(city_bytes, Ordering::Release);
    GEO_ASN_BYTES.store(asn_bytes, Ordering::Release);
    GEO_ASN_AVAILABLE.store(asn_loaded, Ordering::Release);
    GEO_CITY_GENERATION.store(generation, Ordering::Release);
    GEO_AVAILABLE.store(store.current().is_some(), Ordering::Release);
    GEO_CACHE.clear();
    GEO_STORE.store(Some(store));
}

fn active_geo_view() -> Option<Arc<dyn GeoView>> {
    GEO_STORE
        .load_full()
        .and_then(|store| store.current().map(|snapshot| snapshot as Arc<dyn GeoView>))
}

const CACHE_SHARDS: usize = 64;
const GEO_CACHE_ENTRY_ESTIMATED_BYTES: u64 = 384;
const UA_CACHE_ENTRY_ESTIMATED_BYTES: u64 = 768;

fn cache_capacity_per_shard(estimated_entry_bytes: u64, budget_share: u64) -> usize {
    let budget = crate::memory_governor::MEMORY_GOVERNOR
        .snapshot(crate::memory_governor::MEMORY_GOVERNOR.pingora_worker_threads())
        .cardinality_state_budget_bytes
        / budget_share.max(1);
    usize::try_from(budget / estimated_entry_bytes.max(1) / CACHE_SHARDS as u64)
        .unwrap_or(usize::MAX)
        .max(1)
}

struct ShardedLru<K, V> {
    shards: Vec<Mutex<LruCache<K, V>>>,
}

impl<K: Hash + Eq, V: Clone> ShardedLru<K, V> {
    fn new(capacity_per_shard: usize) -> Self {
        let mut shards = Vec::with_capacity(CACHE_SHARDS);
        for _ in 0..CACHE_SHARDS {
            shards.push(Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity_per_shard).unwrap(),
            )));
        }
        Self { shards }
    }

    fn clear(&self) {
        for shard in &self.shards {
            shard.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    fn clear_counted(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                let mut cache = shard.lock().unwrap_or_else(|e| e.into_inner());
                let removed = cache.len();
                cache.clear();
                removed
            })
            .sum()
    }

    fn get_shard(&self, key: &K) -> &Mutex<LruCache<K, V>> {
        let mut s = DefaultHasher::new();
        key.hash(&mut s);
        let hash = s.finish();
        &self.shards[(hash as usize) % CACHE_SHARDS]
    }

    fn get_shard_by<Q>(&self, key: &Q) -> &Mutex<LruCache<K, V>>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut s = DefaultHasher::new();
        key.hash(&mut s);
        let hash = s.finish();
        &self.shards[(hash as usize) % CACHE_SHARDS]
    }
}

// Cache for GeoIP results (IP -> (bundle generation, GeoInfo)). The generation
// is part of the key: a cached answer from a previous bundle must never be
// served after an activation, even if the cache clear races with this lookup.
static GEO_CACHE: Lazy<ShardedLru<IpAddr, (u64, Option<GeoInfo>)>> =
    Lazy::new(|| ShardedLru::new(cache_capacity_per_shard(GEO_CACHE_ENTRY_ESTIMATED_BYTES, 4)));

// Cache for User-Agent results (UA string -> (Arc<str>, Arc<str>))
type UaCacheValue = (Arc<str>, Arc<str>, Arc<str>, Arc<str>);
static UA_CACHE: Lazy<ShardedLru<String, UaCacheValue>> =
    Lazy::new(|| ShardedLru::new(cache_capacity_per_shard(UA_CACHE_ENTRY_ESTIMATED_BYTES, 4)));

static UA_PARSER: Lazy<Parser> = Lazy::new(Parser::new);

fn cached_geo(ip: IpAddr) -> Option<GeoInfo> {
    let view = active_geo_view()?;
    let generation = view.generation();
    let mutex = GEO_CACHE.get_shard(&ip);
    let mut cache = mutex.lock().unwrap_or_else(|e| e.into_inner());
    match cache.get(&ip) {
        Some((cached_generation, cached)) if *cached_generation == generation => cached.clone(),
        _ => {
            let result = lookup_geo_on(view.as_ref(), ip);
            cache.put(ip, (generation, result.clone()));
            result
        }
    }
}

pub fn analyze_request(ip: IpAddr, ua: &str) -> RequestStats {
    let geo = cached_geo(ip);

    let (browser, browser_version, os, os_version) = if ua.is_empty() {
        (Arc::from(""), Arc::from(""), Arc::from(""), Arc::from(""))
    } else {
        let mutex = UA_CACHE.get_shard_by(ua);
        let mut cache = mutex.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(ua) {
            cached.clone()
        } else {
            let parsed_ua = UA_PARSER.parse(ua);
            let res = match parsed_ua {
                Some(p) => (
                    Arc::from(p.name),
                    Arc::from(p.version),
                    Arc::from(p.os),
                    Arc::from(p.os_version.as_ref()),
                ),
                None => (
                    Arc::from("Unknown"),
                    Arc::from(""),
                    Arc::from("Unknown"),
                    Arc::from(""),
                ),
            };
            cache.put(ua.to_string(), res.clone());
            res
        }
    };

    RequestStats {
        geo,
        browser,
        browser_version,
        os,
        os_version,
    }
}

fn lookup_geo_on(view: &dyn GeoView, ip: IpAddr) -> Option<GeoInfo> {
    let lookup = view.lookup_full(ip);
    let record = lookup.record?;
    Some(GeoInfo {
        country: Arc::from(
            record
                .country_name
                .or_else(|| record.country_iso.clone())
                .unwrap_or_default(),
        ),
        country_id: record.country_id.map(i64::from).unwrap_or(0),
        country_iso: Arc::from(record.country_iso.unwrap_or_default()),
        region: Arc::from(
            record
                .subdivision_name
                .or_else(|| record.subdivision_iso.clone())
                .unwrap_or_default(),
        ),
        region_id: record.subdivision_id.map(i64::from).unwrap_or(0),
        region_iso: Arc::from(record.subdivision_iso.unwrap_or_default()),
        city: Arc::from(record.city_name.unwrap_or_default()),
        city_id: record.city_id.map(i64::from).unwrap_or(0),
        provider: Arc::from(record.asn_org.unwrap_or_else(|| "Unknown".to_string())),
    })
}

pub fn lookup_isp_name(ip: IpAddr) -> Arc<str> {
    active_geo_view()
        .and_then(|view| view.lookup_asn(ip))
        .and_then(|(_, org)| org)
        .map(Arc::from)
        .unwrap_or_else(|| Arc::from("Unknown"))
}

pub fn lookup_asn_number(ip: IpAddr) -> i64 {
    active_geo_view()
        .and_then(|view| view.lookup_asn(ip))
        .map(|(number, _)| i64::from(number))
        .unwrap_or(0)
}

pub fn lookup_asn_label(ip: IpAddr) -> Arc<str> {
    let number = lookup_asn_number(ip);
    if number > 0 {
        Arc::from(format!("AS{number}"))
    } else {
        Arc::from("")
    }
}

pub fn asn_database_available() -> bool {
    GEO_ASN_AVAILABLE.load(Ordering::Acquire)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GeoIpMemorySnapshot {
    pub city_loaded: bool,
    pub city_bytes: u64,
    pub city_generation: u64,
    pub asn_loaded: bool,
    pub asn_bytes: u64,
}

pub fn geoip_memory_snapshot() -> GeoIpMemorySnapshot {
    GeoIpMemorySnapshot {
        city_loaded: GEO_AVAILABLE.load(Ordering::Acquire),
        city_bytes: GEO_CITY_BYTES.load(Ordering::Acquire),
        city_generation: GEO_CITY_GENERATION.load(Ordering::Acquire),
        asn_loaded: asn_database_available(),
        asn_bytes: GEO_ASN_BYTES.load(Ordering::Acquire),
    }
}

pub fn lookup_geo(ip: IpAddr) -> Option<GeoInfo> {
    cached_geo(ip)
}

pub fn reclaim_geo_ua_caches(clear_all: bool) -> (usize, usize) {
    let geo_removed = GEO_CACHE.clear_counted();
    let ua_removed = if clear_all {
        UA_CACHE.clear_counted()
    } else {
        0
    };
    (geo_removed, ua_removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_request_reuses_ua_cache_result_shape() {
        let ip = "127.0.0.1".parse().expect("valid ip");
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";

        let first = analyze_request(ip, ua);
        let second = analyze_request(ip, ua);

        assert_eq!(&*first.browser, &*second.browser);
        assert_eq!(&*first.os, &*second.os);
    }

    #[test]
    fn edge_geo_info_reads_names_and_ids_from_one_snapshot_record() {
        struct View;
        impl zelvik_geo_runtime::GeoView for View {
            fn lookup(&self, _addr: IpAddr) -> zelvik_geo_runtime::GeoLookup {
                zelvik_geo_runtime::GeoLookup {
                    record: Some(zelvik_geo_runtime::GeoRecord {
                        country_iso: Some("CN".to_string()),
                        country_id: Some(1814991),
                        country_name: Some("China".to_string()),
                        subdivision_iso: Some("BJ".to_string()),
                        subdivision_id: Some(2038349),
                        subdivision_name: Some("Beijing".to_string()),
                        city_name: Some("Beijing".to_string()),
                        city_id: Some(1816670),
                        asn: Some(64500),
                        asn_org: Some("Example Network".to_string()),
                    }),
                    city_matched_net: Some("1.2.3.0/24".parse().unwrap()),
                    country_matched_net: Some("1.2.3.0/24".parse().unwrap()),
                    asn_matched_net: Some("1.2.0.0/16".parse().unwrap()),
                    generation: 9,
                    bundle_id: Arc::from("gb_edge"),
                }
            }
            fn match_cidr_set(
                &self,
                _addr: IpAddr,
                _set: &str,
            ) -> Option<zelvik_geo_runtime::IgpMatch> {
                None
            }
            fn cidr_set_names(&self) -> Vec<&str> {
                Vec::new()
            }
            fn has_asn(&self) -> bool {
                true
            }
            fn generation(&self) -> u64 {
                9
            }
            fn bundle_id(&self) -> &str {
                "gb_edge"
            }
        }

        let info = lookup_geo_on(&View, "1.2.3.4".parse().unwrap()).expect("record");
        assert_eq!(&*info.country, "China");
        assert_eq!(info.country_id, 1814991);
        assert_eq!(&*info.country_iso, "CN");
        assert_eq!(&*info.region, "Beijing");
        assert_eq!(info.region_id, 2038349);
        assert_eq!(&*info.city, "Beijing");
        assert_eq!(info.city_id, 1816670);
        assert_eq!(&*info.provider, "Example Network");
    }

    #[test]
    fn lookup_asn_label_formats_as_prefix_when_number_known() {
        assert_eq!(lookup_asn_label("127.0.0.1".parse().unwrap()).as_ref(), "");
        assert_eq!(lookup_asn_number("127.0.0.1".parse().unwrap()), 0);
    }
}
