//! Paired component probe; compile through mbx against the matching HTTP and
//! mimalloc artifacts. This measures extraction, not HTTP proxy throughput.
#[path = "../../src/metrics/template.rs"]
pub mod metric_template;
#[path = "../../src/metrics/request_fields.rs"]
pub mod request_fields;
use request_fields::RequestFields;
use std::alloc::{GlobalAlloc, Layout};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hint::black_box;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

struct Counted;
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
#[global_allocator]
static ALLOCATOR: Counted = Counted;
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { mimalloc::MiMalloc.alloc(layout) };
        if !p.is_null() && COUNT.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { mimalloc::MiMalloc.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { mimalloc::MiMalloc.realloc(p, layout, size) };
        if !result.is_null() && COUNT.load(Ordering::Relaxed) {
            REALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}
fn timed(f: &mut impl FnMut()) -> f64 {
    let t = Instant::now();
    for _ in 0..10_000 {
        f();
    }
    t.elapsed().as_nanos() as f64 / 10_000.0
}
fn allocated(f: &mut impl FnMut()) -> (f64, f64) {
    ALLOCS.store(0, Ordering::Relaxed);
    REALLOCS.store(0, Ordering::Relaxed);
    COUNT.store(true, Ordering::Relaxed);
    for _ in 0..1_000 {
        f();
    }
    COUNT.store(false, Ordering::Relaxed);
    (
        ALLOCS.load(Ordering::Relaxed) as f64 / 1000.0,
        REALLOCS.load(Ordering::Relaxed) as f64 / 1000.0,
    )
}
fn pair(name: &str, mut old: impl FnMut(), mut new: impl FnMut()) {
    for _ in 0..100 {
        old();
        new();
    }
    let mut a = Vec::new();
    let mut b = Vec::new();
    for i in 0..7 {
        if i % 2 == 0 {
            a.push(timed(&mut old));
            b.push(timed(&mut new));
        } else {
            b.push(timed(&mut new));
            a.push(timed(&mut old));
        }
    }
    let old_alloc = allocated(&mut old);
    let new_alloc = allocated(&mut new);
    let mut sa = a.clone();
    sa.sort_by(f64::total_cmp);
    let mut sb = b.clone();
    sb.sort_by(f64::total_cmp);
    println!(
        "{{\"name\":\"{name}\",\"old_ns_rounds\":{a:?},\"new_ns_rounds\":{b:?},\"old_median_ns\":{},\"new_median_ns\":{},\"old_allocations\":{},\"new_allocations\":{},\"old_reallocations\":{},\"new_reallocations\":{}}}",
        sa[3], sb[3], old_alloc.0, new_alloc.0, old_alloc.1, new_alloc.1
    );
}
fn legacy_host(vars: &BTreeSet<Arc<str>>, host: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let wanted = vars.iter().any(|name| {
        name.starts_with("host.")
            || matches!(
                name.as_ref(),
                "host" | "serverName" | "cname" | "requestURL"
            )
    });
    let parts = if wanted {
        host.split('.').collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if let Some(first) = parts.first() {
        for key in ["host.first", "host.0"] {
            if vars.contains(key) {
                values.insert(key.to_string(), first.to_string());
            }
        }
    }
    if let Some(last) = parts.last() {
        for key in ["host.last", "host.-1"] {
            if vars.contains(key) {
                values.insert(key.to_string(), last.to_string());
            }
        }
    }
    for idx in 1..=4 {
        let forward = format!("host.{idx}");
        let backward = format!("host.-{}", idx + 1);
        if let Some(part) = parts.get(idx)
            && vars.contains(forward.as_str())
        {
            values.insert(forward, part.to_string());
        }
        if parts.len() > idx && vars.contains(backward.as_str()) {
            values.insert(backward, parts[parts.len() - idx - 1].to_string());
        }
    }
    values
}
fn legacy_headers(
    vars: &BTreeSet<Arc<str>>,
    headers: &http::HeaderMap,
) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let mut requested = Vec::new();
    for name in vars {
        for prefix in ["header.", "http."] {
            if let Some(suffix) = name.strip_prefix(prefix) {
                requested.push((suffix, prefix));
            }
        }
    }
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            for (suffix, prefix) in &requested {
                if suffix.eq_ignore_ascii_case(name.as_str()) {
                    values
                        .entry(format!("{prefix}{name}"))
                        .or_insert_with(|| value.to_string());
                }
            }
        }
    }
    if vars.contains("headers") {
        values.insert(
            "headers".to_string(),
            headers
                .iter()
                .filter_map(|(name, value)| {
                    value.to_str().ok().map(|value| format!("{name}: {value}"))
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    values
}
fn legacy_response(
    vars: &BTreeSet<Arc<str>>,
    headers: &HashMap<String, String>,
) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for (name, value) in headers {
        if vars.contains("response.contentType") && name.eq_ignore_ascii_case("content-type") {
            values.insert("response.contentType".to_string(), value.clone());
        }
        let key = format!("response.header.{name}");
        if vars.iter().any(|v| v.eq_ignore_ascii_case(&key)) {
            values.insert(key, value.clone());
        }
    }
    values
}
fn set(names: &[&str]) -> BTreeSet<Arc<str>> {
    names.iter().copied().map(Arc::from).collect()
}
fn legacy_template<F>(source: &str, mut resolve_variable: F) -> String
where
    F: FnMut(&str) -> String,
{
    let Some(mut cursor) = source.find("${") else {
        return source.to_string();
    };

    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..cursor]);

    while cursor < source.len() {
        let var_start = cursor + 2;
        let Some(relative_end) = source[var_start..].find('}') else {
            out.push_str(&source[cursor..]);
            return out;
        };
        let var_end = var_start + relative_end;
        out.push_str(&resolve_variable(&source[var_start..var_end]));

        let next_start = var_end + 1;
        if let Some(next_relative) = source[next_start..].find("${") {
            cursor = next_start + next_relative;
            out.push_str(&source[next_start..cursor]);
        } else {
            out.push_str(&source[next_start..]);
            return out;
        }
    }

    out
}

fn main() {
    let mut registered = RequestFields::default();
    registered.register(Arc::from("host"));
    registered.finish();
    assert!(
        !registered.wants_arguments()
            && !registered.wants_cookies()
            && !registered.wants_origin()
            && !registered.wants_response_headers()
    );
    assert_eq!(registered.variables().len(), 1);
    let host = "alpha.beta.example.test";
    for (label, names) in [
        (
            "unreferenced_host_parts",
            vec!["host", "request.method", "status"],
        ),
        (
            "selected_host_parts",
            vec!["host.1", "host.-1", "host.first"],
        ),
    ] {
        let vars = set(&names);
        let fields = RequestFields::compile(vars.clone());
        let project = || {
            let mut m = BTreeMap::new();
            black_box(&fields).project_host_parts(black_box(host), &mut m);
            m
        };
        assert_eq!(legacy_host(&vars, host), project());
        pair(
            label,
            || {
                black_box(legacy_host(black_box(&vars), black_box(host)));
            },
            || {
                black_box(project());
            },
        );
    }
    let mut headers = http::HeaderMap::new();
    let mut response = HashMap::new();
    for i in 0..32 {
        let name = format!("x-field-{i}");
        headers.insert(
            http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            http::HeaderValue::from_static("example-value"),
        );
        response.insert(name, "example-value".to_string());
    }
    for (label, names) in [
        (
            "two_selected_headers",
            vec!["header.X-Field-1", "header.x-field-20"],
        ),
        ("complete_headers", vec!["headers"]),
    ] {
        let vars = set(&names);
        let fields = RequestFields::compile(vars.clone());
        let project = || {
            let mut m = BTreeMap::new();
            black_box(&fields).project_headers(black_box(&headers), &mut m);
            m
        };
        assert_eq!(legacy_headers(&vars, &headers), project());
        pair(
            label,
            || {
                black_box(legacy_headers(black_box(&vars), black_box(&headers)));
            },
            || {
                black_box(project());
            },
        );
    }
    let vars = set(&["response.header.X-Field-1"]);
    let fields = RequestFields::compile(vars.clone());
    let project = || {
        let mut m = BTreeMap::new();
        black_box(&fields).project_response_headers(black_box(&response), &mut m);
        m
    };
    assert_eq!(legacy_response(&vars, &response), project());
    pair(
        "one_selected_response_header",
        || {
            black_box(legacy_response(black_box(&vars), black_box(&response)));
        },
        || {
            black_box(project());
        },
    );
    let vars = set(&["cookie.session"]);
    let fields = RequestFields::compile(vars.clone());
    let mut cookies = (0..32)
        .map(|i| format!("ignored{i}=value"))
        .collect::<Vec<_>>()
        .join(";");
    cookies.push_str(";session=selected");
    let old = || {
        let mut m = BTreeMap::new();
        for cookie in black_box(&cookies).split(';') {
            if let Some((name, value)) = cookie.split_once('=') {
                let key = format!("cookie.{}", name.trim());
                if vars.contains(key.as_str()) {
                    m.insert(key, value.trim().to_string());
                }
            }
        }
        m
    };
    let new = || {
        let mut m = BTreeMap::new();
        for cookie in black_box(&cookies).split(';') {
            if let Some((name, value)) = cookie.split_once('=')
                && let Some(key) = black_box(&fields).cookie_key(name.trim())
            {
                m.insert(key.to_string(), value.trim().to_string());
            }
        }
        m
    };
    assert_eq!(old(), new());
    pair(
        "selected_cookie_among_32_ignored",
        || {
            black_box(old());
        },
        || {
            black_box(new());
        },
    );
    let args = RequestFields::compile(set(&["arg.q"]));
    assert_eq!(args.argument_key("q"), Some("arg.q"));
    let attrs = BTreeMap::from([
        ("host", "long.example.test".to_string()),
        ("status", "200".to_string()),
        ("country", "CN".to_string()),
    ]);
    for (label, source) in [
        ("pure_variable_template", "${host}"),
        (
            "mixed_template",
            "host=${host};status=${status};country=${country}",
        ),
    ] {
        let compiled = metric_template::CompiledTemplate::compile(Arc::from(source));
        assert!(compiled.variables().next().is_some());
        let old = || {
            legacy_template(black_box(source), |name| {
                black_box(&attrs).get(name).cloned().unwrap_or_default()
            })
        };
        let new = || {
            black_box(&compiled)
                .render(|name| black_box(&attrs).get(name).cloned().unwrap_or_default())
        };
        assert_eq!(old(), new());
        pair(
            label,
            || {
                black_box(old());
            },
            || {
                black_box(new());
            },
        );
    }
}
