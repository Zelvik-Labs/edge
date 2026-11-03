use edge_waf_state_probe::bounded_map::BoundedScopedMap;
use serde_json::json;
use std::error::Error;
use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

type Key = (i64, IpAddr);
const TABLES: usize = 7;
const LOOKUPS: usize = 200_000;

fn resident_bytes(page_size: u64) -> Result<u64, Box<dyn Error>> {
    let statm = std::fs::read_to_string("/proc/self/statm")?;
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .ok_or("statm lacks resident pages")?
        .parse()?;
    Ok(pages.checked_mul(page_size).ok_or("RSS overflow")?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("expected capacity, repeat and actual page size".into());
    }
    let capacity: usize = args[1].parse()?;
    let repeat: usize = args[2].parse()?;
    let page_size: u64 = args[3].parse()?;
    if !(65_536..=8_000_000).contains(&capacity) || page_size == 0 {
        return Err("capacity or page size outside probe contract".into());
    }
    let keys: Vec<Key> = (0..TABLES)
        .map(|index| (index as i64, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))))
        .collect();
    let rss_before = resident_bytes(page_size)?;
    let begin = Instant::now();
    let maps: Vec<BoundedScopedMap<Key>> = (0..TABLES)
        .map(|_| BoundedScopedMap::new(capacity))
        .collect();
    let construction_ns = begin.elapsed().as_nanos();
    let rss_empty = resident_bytes(page_size)?;
    assert!(maps.iter().all(BoundedScopedMap::is_empty));

    let begin = Instant::now();
    let mut visits = 0usize;
    for map in &maps {
        map.for_each(|_, _| {
            visits += 1;
            true
        });
    }
    let empty_visit_ns = begin.elapsed().as_nanos();
    assert_eq!(visits, 0);
    let begin = Instant::now();
    for map in &maps {
        map.retain(|_| {
            visits += 1;
            true
        });
    }
    let empty_retain_ns = begin.elapsed().as_nanos();
    assert_eq!(visits, 0);

    let begin = Instant::now();
    let mut misses = 0;
    for index in 0..LOOKUPS {
        let table = index % TABLES;
        misses += usize::from(black_box(maps[table].get(black_box(&keys[table]))).is_none());
    }
    let empty_lookup_ns = begin.elapsed().as_nanos();
    assert_eq!(misses, LOOKUPS);
    let mut first_write_ns = Vec::new();
    for (map, key) in maps.iter().zip(&keys) {
        let begin = Instant::now();
        let evicted = map.insert(*key, 86_400, 0, capacity);
        first_write_ns.push(begin.elapsed().as_nanos());
        assert!(evicted.is_none());
        assert_eq!(map.get(key), Some(86_400));
        assert_eq!(map.len(), 1);
    }
    let rss_one_entry_per_table = resident_bytes(page_size)?;
    let begin = Instant::now();
    let mut hits = 0;
    for index in 0..LOOKUPS {
        let table = index % TABLES;
        hits += usize::from(black_box(maps[table].get(black_box(&keys[table]))) == Some(86_400));
    }
    let hit_lookup_ns = begin.elapsed().as_nanos();
    assert_eq!(hits, LOOKUPS);

    let slot_size = std::mem::size_of::<Option<(Key, i64)>>();
    let physical_slots = (capacity.max(1) / 64).max(64) * 64;
    let record = json!({
        "capacity_requested": capacity, "repeat": repeat,
        "tables": TABLES, "segments_per_table": 64,
        "physical_slots_per_table": physical_slots,
        "slot_size_bytes": slot_size,
        "slot_alignment_bytes": std::mem::align_of::<Option<(Key, i64)>>(),
        "slot_payload_bytes_all_tables": physical_slots * slot_size * TABLES,
        "page_size": page_size, "rss_before": rss_before, "rss_empty": rss_empty,
        "rss_live_empty_delta_bytes": i128::from(rss_empty)-i128::from(rss_before),
        "rss_one_entry_per_table": rss_one_entry_per_table,
        "construction_ns": construction_ns,
        "empty_visit_ns": empty_visit_ns, "empty_retain_ns": empty_retain_ns,
        "lookup_iterations": LOOKUPS, "empty_lookup_ns": empty_lookup_ns,
        "hit_lookup_ns": hit_lookup_ns, "first_write_ns": first_write_ns,
        "verified_misses": misses, "verified_hits": hits,
        "live_entries": maps.iter().map(BoundedScopedMap::len).sum::<usize>(),
        "allocator": "Rust default/System; not Edge mimalloc",
        "scope": "unchanged production table source; representative fixed capacities, not the full node governor or network"
    });
    println!("{}", serde_json::to_string(&record)?);
    black_box(&maps);
    Ok(())
}
