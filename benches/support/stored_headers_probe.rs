//! Exact original header-validation helpers versus a candidate streaming walk.
use http::HeaderMap;
use http::header::{HeaderName, HeaderValue};
use std::hint::black_box;
use std::time::{Duration, Instant};

include!("original.rs");
include!("candidate.rs");
include!("allocator.rs");

type Headers = Vec<(String, String)>;
type Validator = fn(&[(String, String)]) -> bool;

fn headers(items: &[(&str, &str)]) -> Headers {
    items
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn fixtures() -> Vec<(&'static str, Headers)> {
    let normal = headers(&[
        ("server", "nginx/1.28.0"),
        ("date", "Wed, 04 Nov 2026 00:00:00 GMT"),
        ("content-type", "text/plain"),
        ("cache-control", "public, max-age=3600"),
        ("etag", "\"cached-object\""),
        ("last-modified", "Tue, 03 Nov 2026 00:00:00 GMT"),
    ]);
    let mut custom = normal.clone();
    for index in 0..10 {
        custom.push((format!("X-Application-{index}"), "a".repeat(128)));
    }
    let mut private = normal.clone();
    private.push(("Cache-Control".into(), "PrIvAtE=\"authorization\"".into()));
    let mut duplicates = normal.clone();
    duplicates.extend(headers(&[
        ("content-length", "1024"),
        ("Content-Length", "+01024"),
    ]));
    vec![
        ("empty", vec![]),
        ("typical6", normal),
        ("custom16", custom),
        ("private_tail", private),
        ("duplicate_length", duplicates),
        (
            "framing_reject",
            headers(&[("transfer-encoding", "chunked")]),
        ),
        (
            "boundary32",
            (0..32)
                .map(|i| (format!("x-{i}"), "value".into()))
                .collect(),
        ),
        (
            "fallback33",
            (0..33)
                .map(|i| (format!("x-{i}"), "value".into()))
                .collect(),
        ),
    ]
}

fn verify() {
    let mut count = 0u64;
    let mut check = |items: &[(String, String)]| {
        let old = baseline(items);
        let new = candidate(items);
        assert_eq!(old, new, "case {count}: {items:?}");
        println!(
            "{count}\t{}\t{}\t{}",
            items.len(),
            u8::from(old),
            u8::from(new)
        );
        count += 1;
    };
    println!("case\theaders\tbaseline\tcandidate");
    for (_, items) in fixtures() {
        check(&items);
    }
    let names = [
        "",
        "x-value",
        "X-VALUE",
        "content-length",
        "Content-Length",
        "cache-control",
        "Cache-Control",
        "pragma",
        "vary",
        "set-cookie",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
        "content-range",
        "bad name",
        ":status",
        "host",
        "content-encoding",
    ];
    let values = [
        "",
        "0",
        "42",
        "43",
        " 42\t",
        "+042",
        "-1",
        "42,42",
        "18446744073709551615",
        "18446744073709551616",
        "public, max-age=3600",
        "private",
        "PrIvAtE = x",
        "no-cache=\"x\"",
        "no-store",
        "no-store-extra",
        "foo,no-cache",
        "foo=private",
        "chunked",
        "gzip,chunked",
        "value\r\nforged: yes",
        "x\0y",
        "\u{7f}",
        "中文",
        "\u{85}",
        "\t",
    ];
    for name in names {
        for value in values {
            check(&headers(&[(name, value)]));
            for second in values {
                check(&headers(&[(name, value), (name, second)]));
                check(&headers(&[
                    ("Content-Length", "42"),
                    (name, value),
                    ("content-length", second),
                ]));
            }
        }
    }
    for byte in 0..=127u8 {
        let text = char::from(byte).to_string();
        check(&headers(&[(&format!("x{text}y"), "value")]));
        check(&headers(&[("x-value", &format!("x{text}y"))]));
    }
    for length in [31, 32, 33, 128] {
        for (name, value) in [
            ("vary", ""),
            ("content-length", "1"),
            ("X-Ok", "yes"),
            ("x-bad", "\r"),
        ] {
            let mut items: Headers = (0..length)
                .map(|i| (format!("x-{i}"), "value".into()))
                .collect();
            items[length - 1] = (name.into(), value.into());
            check(&items);
        }
    }
    for length in [255, 256, 1024, 32767, 32768, 65535, 65536] {
        check(&[("X".repeat(length), "value".into())]);
    }
}

fn measure(validator: Validator) {
    println!("case\tblock\titerations\tnanoseconds");
    for (name, items) in fixtures() {
        for _ in 0..1000 {
            black_box(validator(black_box(&items)));
        }
        for block in 0..20 {
            let started = Instant::now();
            let mut iterations = 0u64;
            loop {
                for _ in 0..256 {
                    black_box(validator(black_box(&items)));
                }
                iterations += 256;
                if started.elapsed() >= Duration::from_millis(10) {
                    break;
                }
            }
            println!(
                "{name}\t{block}\t{iterations}\t{}",
                started.elapsed().as_nanos()
            );
        }
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "verify" {
        verify();
        return;
    }
    let validator: Validator = match args[2].as_str() {
        "baseline" => baseline,
        "candidate" => candidate,
        _ => panic!("unknown variant"),
    };
    match args[1].as_str() {
        "timing" => measure(validator),
        #[cfg(feature = "count-allocations")]
        "allocations" => {
            println!("case\tallocations\treallocations\tbytes\tlargest");
            for (name, items) in fixtures() {
                allocations::start();
                black_box(validator(black_box(&items)));
                let [count, realloc, bytes, largest] = allocations::finish();
                println!("{name}\t{count}\t{realloc}\t{bytes}\t{largest}");
            }
        }
        _ => panic!("unknown mode"),
    }
}
