//! Exact ConfigStore normalization with borrowed lookup keys, including malformed inputs.
use std::hint::black_box;
use std::time::Instant;
include!("original.rs");
include!("candidate.rs");
include!("allocator.rs");

fn fixtures() -> Vec<(&'static str, String)> {
    vec![
        ("lower", "cdn.example.com".into()),
        ("mixed", "CDN.ExAmPlE.COM".into()),
        ("port", "CDN.ExAmPlE.COM:443".into()),
        ("tail-dots", "CDN.ExAmPlE.COM...:443".into()),
        ("userinfo", "  old@name@  CDN.ExAmPlE.COM.:443  ".into()),
        ("ipv6-bracket", "[2001:0DB8::1]:443".into()),
        ("ipv6-bare", "2001:0DB8::1".into()),
        ("unmatched", "[A:B.C".into()),
        ("unicode", "ÉXAMPLE.İ.TEST:443".into()),
        ("long-upper", format!("{}.EXAMPLE.COM:443", "A".repeat(240))),
        ("long-lower", format!("{}.example.com:443", "a".repeat(240))),
        ("empty", "".into()),
        ("spaces", "  ".into()),
        ("all-dots", "...".into()),
        ("reparsed-colon", "a:b".into()),
        ("bracket-non-ip", "[A:B]:443".into()),
    ]
}

fn verify() {
    println!("case\tbaseline\tcandidate\tborrowed");
    let mut cases: Vec<String> = fixtures().into_iter().map(|(_, value)| value).collect();
    for value in [
        "",
        ":",
        "@",
        "[]",
        "[]:443",
        "[::1]garbage",
        "[::1]:",
        "[ABC",
        "]x:1",
        "a:b:c",
        "a:b",
        "a:",
        ":443",
        "a..",
        "a.:x",
        "a@b@c",
        "\0A:1",
        "\u{2003}A\u{2003}",
        "A\u{3002}B",
    ] {
        cases.push(value.to_string());
    }
    let alphabet = [
        'A', 'z', '.', ':', '[', ']', '@', ' ', '\t', '\n', '\0', 'é', 'İ', 'ß', '中', '\u{2003}',
        '\u{3002}', '1',
    ];
    let mut seed = 20261104_u64;
    for index in 0..65536 {
        let mut value = String::new();
        for _ in 0..index % 193 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            value.push(alphabet[(seed >> 32) as usize % alphabet.len()]);
        }
        cases.push(value);
    }
    for position in 0..258 {
        let mut value = "a".repeat(258);
        value.replace_range(position..position + 1, "A");
        cases.push(value);
    }
    cases.push("a".repeat(258));
    cases.push("é".repeat(129));
    for (i, value) in cases.iter().enumerate() {
        let a = baseline(value);
        let b = candidate(value);
        assert_eq!(a, b, "input {value:?}");
        assert_eq!(
            baseline_strip_addr_port(value),
            candidate_strip_addr_port(value)
        );
        let stripped = candidate_strip_addr_port(value).trim_end_matches('.');
        let should_borrow = !stripped.as_bytes().iter().any(u8::is_ascii_uppercase);
        let borrowed = matches!(b, std::borrow::Cow::Borrowed(_));
        assert_eq!(borrowed, should_borrow);
        println!("{i}\t{a:?}\t{b:?}\t{borrowed}");
    }
}

fn timing(resolve: for<'a> fn(&'a str) -> std::borrow::Cow<'a, str>) {
    println!("case\tblock\titerations\tnanoseconds");
    for (name, value) in fixtures() {
        for _ in 0..1000 {
            black_box(resolve(black_box(&value)));
        }
        for block in 0..20 {
            let iterations = 100_000;
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(resolve(black_box(&value)));
            }
            println!(
                "{name}\t{block}\t{iterations}\t{}",
                start.elapsed().as_nanos()
            );
        }
    }
}

#[cfg(feature = "count-allocations")]
fn allocation_counts(resolve: for<'a> fn(&'a str) -> std::borrow::Cow<'a, str>) {
    println!("case\titerations\tallocations\treallocations\tbytes\tlargest");
    for (name, value) in fixtures() {
        let iterations = 1000;
        allocations::start();
        for _ in 0..iterations {
            black_box(resolve(black_box(&value)));
        }
        let [count, realloc, bytes, largest] = allocations::finish();
        println!("{name}\t{iterations}\t{count}\t{realloc}\t{bytes}\t{largest}");
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "verify" {
        verify();
        return;
    }
    let resolve = match args[2].as_str() {
        "baseline" => baseline,
        "candidate" => candidate,
        _ => panic!("unknown variant"),
    };
    match args[1].as_str() {
        "timing" => timing(resolve),
        #[cfg(feature = "count-allocations")]
        "allocations" => allocation_counts(resolve),
        _ => panic!("unknown mode"),
    }
}
