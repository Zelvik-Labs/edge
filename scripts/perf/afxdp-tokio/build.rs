use std::env;
use std::path::PathBuf;

fn main() {
    assert_eq!(env::var("CARGO_CFG_TARGET_OS").unwrap(), "linux");
    println!("cargo:rerun-if-changed=native.c");
    println!("cargo:rerun-if-changed=../afxdp-wake.c");
    let legacy_source = "../../../src/xdp/af_xdp/wake.rs";
    println!("cargo:rerun-if-changed={legacy_source}");
    // A harness=false bench is compiled with cfg(test), but does not build
    // the #[test] functions. Include only runtime items here; src/lib.rs
    // separately includes the original complete module for its real tests.
    let source = std::fs::read_to_string(legacy_source).unwrap();
    let (runtime, _) = source
        .split_once("\n#[cfg(test)]\nmod tests {")
        .expect("legacy test boundary changed");
    let runtime: String = runtime
        .lines()
        .map(|line| {
            let line = line
                .strip_prefix("//!")
                .map_or_else(|| line.to_owned(), |comment| format!("//{comment}"));
            format!("{line}\n")
        })
        .collect();
    std::fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("legacy_worker.rs"),
        runtime,
    )
    .unwrap();
    let variables = [
        "EDGE_XDP_BENCH_XDP_INCLUDE",
        "EDGE_XDP_BENCH_BPF_INCLUDE",
        "EDGE_XDP_BENCH_XDP_ARCHIVE",
        "EDGE_XDP_BENCH_BPF_ARCHIVE",
    ];
    for name in variables {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let local = variables.map(env::var_os);
    let configured = local.iter().filter(|value| value.is_some()).count();
    assert!(
        configured == 0 || configured == 4,
        "configure all four native paths or none"
    );
    let mut build = cc::Build::new();
    build
        .file("native.c")
        .flag("-std=gnu11")
        .flag("-O2")
        .warnings_into_errors(true);
    if configured == 4 {
        for value in &local[..2] {
            let path = PathBuf::from(value.as_ref().unwrap());
            assert!(path.is_dir(), "native include path must exist");
            build.include(path);
        }
    } else {
        for library in ["libxdp", "libbpf"] {
            let mut config = pkg_config::Config::new();
            config.cargo_metadata(false);
            if library == "libbpf" {
                config.atleast_version("1.0");
            }
            let native = config
                .probe(library)
                .expect("native AF_XDP development library required");
            for path in native.include_paths {
                build.include(path);
            }
        }
    }
    build.compile("edge_xsk_wake_fixture");
    if configured == 4 {
        for (value, library) in local[2..].iter().zip(["xdp", "bpf"]) {
            let path = PathBuf::from(value.as_ref().unwrap());
            assert!(path.is_file(), "native archive must exist");
            println!(
                "cargo:rustc-link-search=native={}",
                path.parent().unwrap().display()
            );
            println!("cargo:rustc-link-lib=static={library}");
        }
        for library in ["elf", "z", "zstd", "pthread", "dl", "m"] {
            println!("cargo:rustc-link-lib={library}");
        }
    } else {
        pkg_config::probe_library("libxdp").unwrap();
        pkg_config::probe_library("libbpf").unwrap();
    }
}
