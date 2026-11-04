//! Read-only HeaderMap component with exact production IP parsing helpers.
use http::HeaderMap;
use std::hint::black_box;
use std::net::IpAddr;
use std::sync::LazyLock as Lazy;
use std::time::Instant;

pub struct Session {
    headers: HeaderMap,
}

impl Session {
    fn req_header(&self) -> &Self {
        self
    }
}

include!("original.rs");
include!("candidate.rs");
include!("allocator.rs");

type Resolver = fn(&Session, IpAddr) -> IpAddr;

fn session(fields: &[(&str, &[u8])]) -> Session {
    let mut headers = HeaderMap::new();
    for (name, value) in fields {
        headers.append(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::HeaderValue::from_bytes(value).unwrap(),
        );
    }
    Session { headers }
}

fn fixtures() -> Vec<(&'static str, Session, IpAddr)> {
    let private = "10.0.0.1".parse().unwrap();
    let common = [
        ("host", b"example.test".as_slice()),
        ("user-agent", b"edge-bench"),
        ("accept", b"*/*"),
    ];
    let mut first = common.to_vec();
    first.push(("x-cloud-real-ip", b"198.51.100.7"));
    let mut second = common.to_vec();
    second.push(("x-real-ip", b"198.51.100.8"));
    let mut forwarded = common.to_vec();
    forwarded.push(("x-forwarded-for", b"198.51.100.9, 203.0.113.1"));
    let mut large = session(&common);
    for index in 0..24 {
        large.headers.insert(
            http::HeaderName::from_bytes(format!("x-unrelated-{index}").as_bytes()).unwrap(),
            http::HeaderValue::from_static("value"),
        );
    }
    let mut large_forwarded = session(&common);
    for index in 0..24 {
        large_forwarded.headers.insert(
            http::HeaderName::from_bytes(format!("x-unrelated-{index}").as_bytes()).unwrap(),
            http::HeaderValue::from_static("value"),
        );
    }
    large_forwarded.headers.insert(
        "x-forwarded-for",
        http::HeaderValue::from_static("203.0.113.5"),
    );
    vec![
        (
            "public-small",
            session(&common),
            "192.0.2.1".parse().unwrap(),
        ),
        ("private-no-forward3", session(&common), private),
        ("private-first4", session(&first), private),
        ("private-second4", session(&second), private),
        ("private-forwarded4", session(&forwarded), private),
        (
            "private-invalid4",
            session(&[
                ("host", b"example.test"),
                ("x-real-ip", b"invalid"),
                ("x-real-ip", b"203.0.113.2"),
                ("x-forwarded-for", b"198.51.100.3"),
            ]),
            private,
        ),
        (
            "private-priority4",
            session(&[
                ("x-real-ip", b"203.0.113.1"),
                ("x-forwarded-for", b"198.51.100.1"),
                ("x-cloud-real-ip", b"not-an-ip"),
                ("accept", b"*/*"),
            ]),
            private,
        ),
        ("private-large27", large, private),
        ("private-large-forward28", large_forwarded, private),
        (
            "mapped-private4",
            session(&forwarded),
            "::ffff:10.0.0.1".parse().unwrap(),
        ),
    ]
}

fn verify() {
    println!("case\tbaseline\tcandidate");
    let mut count = 0;
    let mut compare = |s: &Session, ip| {
        let old = baseline(s, ip);
        let new = candidate(s, ip);
        assert_eq!(old, new, "case {count}");
        println!("{count}\t{old}\t{new}");
        count += 1;
    };
    for (_, s, ip) in fixtures() {
        compare(&s, ip);
    }
    let ips: Vec<IpAddr> = [
        "192.0.2.1",
        "10.1.2.3",
        "127.0.0.1",
        "fd00::1",
        "::1",
        "::ffff:10.1.2.3",
    ]
    .into_iter()
    .map(|s| s.parse().unwrap())
    .collect();
    let values: &[&[u8]] = &[
        b"198.51.100.2",
        b"203.0.113.5",
        b"2001:db8::2",
        b"::ffff:192.0.2.9",
        b"invalid",
        b"",
        b"  ",
        b"for=198.51.100.9;proto=https",
        b"For=\"203.0.113.9\"",
        b"1.2.3.4, 5.6.7.8",
        b"[2001:db8::3]",
        b"\xff",
    ];
    for header in FALLBACK_IP_HEADERS.iter() {
        for first in values {
            for second in values {
                let s = session(&[
                    (header.as_str(), first),
                    (header.as_str(), second),
                    ("x-forwarded-for", b"198.51.100.77"),
                ]);
                for ip in &ips {
                    compare(&s, *ip);
                }
            }
        }
    }
    let mut seed = 20261104u64;
    for _ in 0..65536 {
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed
        };
        let mut s = session(&[]);
        for _ in 0..next() % 12 {
            let index = (next() % 16) as usize;
            let name = if index < FALLBACK_IP_HEADERS.len() {
                FALLBACK_IP_HEADERS[index].clone()
            } else {
                http::HeaderName::from_static("x-unrelated")
            };
            let value =
                http::HeaderValue::from_bytes(values[(next() % values.len() as u64) as usize])
                    .unwrap();
            s.headers.append(name, value);
        }
        compare(&s, ips[(next() % ips.len() as u64) as usize]);
    }
}

fn timing(resolve: Resolver) {
    println!("case\tblock\titerations\tnanoseconds");
    for (name, s, raw) in fixtures() {
        for _ in 0..1000 {
            black_box(resolve(black_box(&s), black_box(raw)));
        }
        for block in 0..20 {
            let start = Instant::now();
            for _ in 0..100000 {
                black_box(resolve(black_box(&s), black_box(raw)));
            }
            println!("{name}\t{block}\t100000\t{}", start.elapsed().as_nanos());
        }
    }
}

#[cfg(feature = "count-allocations")]
fn count_allocations(resolve: Resolver) {
    println!("case\tallocations\treallocations\tbytes\tlargest");
    for (name, s, raw) in fixtures() {
        black_box(resolve(&s, raw));
        allocations::start();
        for _ in 0..1000 {
            black_box(resolve(black_box(&s), black_box(raw)));
        }
        let [count, reallocations, bytes, largest] = allocations::finish();
        println!("{name}\t{count}\t{reallocations}\t{bytes}\t{largest}");
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "verify" {
        verify();
        return;
    }
    let resolver: Resolver = match args[2].as_str() {
        "baseline" => baseline,
        "candidate" => candidate,
        _ => panic!("unknown variant"),
    };
    match args[1].as_str() {
        "timing" => timing(resolver),
        #[cfg(feature = "count-allocations")]
        "allocations" => count_allocations(resolver),
        _ => panic!("unknown mode"),
    }
}
