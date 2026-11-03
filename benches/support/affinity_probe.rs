//! Exact old/new queue selection and native thread affinity diagnostic.
mod affinity;

#[derive(Default)]
pub(crate) struct XdpConfig {
    interfaces: Vec<Interface>,
}
struct Interface {
    name: String,
    queues: Vec<u32>,
    cpus: Vec<u32>,
}

include!("selection.rs");

fn observed_mask() -> String {
    std::fs::read_to_string("/proc/thread-self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .unwrap()
        .trim()
        .to_owned()
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let variant = args.get(1).expect("choose baseline or candidate");
    let expected: Vec<u32> = std::env::var("EDGE_AFFINITY_EXPECTED")
        .unwrap()
        .split(',')
        .map(|value| value.parse().unwrap())
        .collect();
    let current = affinity::CpuAffinity::current().unwrap();
    let explicit = XdpConfig {
        interfaces: vec![Interface {
            name: "fixture".into(),
            queues: vec![7],
            cpus: vec![*expected.last().unwrap()],
        }],
    };
    assert_eq!(
        baseline_queue_cpu(&explicit, "fixture", 7, 0, expected.len()),
        *expected.last().unwrap()
    );
    assert_eq!(
        candidate_queue_cpu(&explicit, "fixture", 7, 0, &current),
        *expected.last().unwrap()
    );
    let config = XdpConfig::default();
    let selected = match variant.as_str() {
        "baseline" => baseline_queue_cpu(&config, "fixture", 7, 0, expected.len()),
        "candidate" => candidate_queue_cpu(&config, "fixture", 7, 0, &current),
        _ => panic!("unsupported variant"),
    };
    let before = observed_mask();
    if args.get(2).is_some_and(|arg| arg == "--assert-budget") {
        assert!(
            expected.contains(&selected),
            "automatic queue CPU must belong to the inherited affinity"
        );
    }
    let pinned = if variant == "baseline" {
        baseline_pin(selected)
    } else {
        current.pin_current_thread(selected).unwrap();
        true
    };
    println!("variant\tselected\tpinned\tbefore\tafter");
    println!(
        "{variant}\t{selected}\t{pinned}\t{before}\t{}",
        observed_mask()
    );
}
