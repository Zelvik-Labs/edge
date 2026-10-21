//! Node runtime directory resolution.
//!
//! The node resolves `configs/`, `data/` and `logs/` under a single home. The
//! order is fixed and must stay in sync with `deploy/install.sh` and
//! `deploy/systemd/zelvik-edge.service`:
//!
//! 1. `ZELVIK_EDGE_HOME` (the wrapper and the unit both export it)
//! 2. the installed unit's `Environment=ZELVIK_EDGE_HOME=`
//! 3. the installed unit's `WorkingDirectory=`
//! 4. `/opt/zelvik/edge` (installer default)
//! 5. `/var/lib/zelvik/edge` (pre-26.10.3.1 installer default)
//! 6. the current working directory (development checkout)
//!
//! A candidate that is `/`, empty, not a directory, or that has neither a
//! `configs/` nor a `data/` directory is rejected: installed packages always
//! create those, so accepting `/` would scatter state across the filesystem and
//! accepting an empty directory would silently run against the wrong tree.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Installer default home for a fresh node (26.10.3.1 and later).
pub const DEFAULT_EDGE_HOME: &str = "/opt/zelvik/edge";
/// Home used by installers before 26.10.3.1.
pub const LEGACY_EDGE_HOME: &str = "/var/lib/zelvik/edge";
/// Systemd unit path the node inspects for its own home.
pub const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/zelvik-edge.service";
/// Wrapper installed by `deploy/install.sh`.
pub const WRAPPER_PATH: &str = "/usr/local/bin/zelvik-edge";

#[derive(Clone, Debug)]
pub struct NodePaths {
    root: PathBuf,
}

impl NodePaths {
    /// Home used by every runtime path accessor.
    ///
    /// Startup code calls `NodePaths::try_current` and aborts with the full
    /// candidate list when the environment is unusable; this infallible form
    /// exists for the many library call sites that cannot return an error.
    /// When resolution fails it logs the reason once and falls back to the
    /// working directory so the eventual config error still names real paths.
    pub fn current() -> Self {
        match Self::try_current() {
            Ok(paths) => paths,
            Err(err) => {
                tracing::error!("{err:#}");
                Self {
                    root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                }
            }
        }
    }

    /// Resolve the home or explain every candidate that was rejected.
    pub fn try_current() -> anyhow::Result<Self> {
        // An explicit home is adopted as-is once it is a safe absolute path: it
        // may be an installation being created right now, so requiring configs/
        // to already exist would make the wrapper unusable during bootstrap.
        // Discovery candidates below do require an existing layout.
        if let Some(value) = std::env::var_os("ZELVIK_EDGE_HOME") {
            let home = PathBuf::from(value);
            if !is_safe_home_path(&home) {
                anyhow::bail!(
                    "ZELVIK_EDGE_HOME={} is not a safe node home: it must be an absolute path other than '/' containing only letters, digits, '.', '_', '-' and '/'",
                    home.display()
                );
            }
            return Ok(Self::from_root(home));
        }
        let mut candidates: Vec<(&str, PathBuf)> = Vec::new();
        let service = systemd_unit_text();
        if let Some(home) = service
            .as_deref()
            .and_then(service_env_zelvik_edge_home_from_text)
        {
            candidates.push(("systemd Environment=ZELVIK_EDGE_HOME", home));
        }
        if let Some(dir) = service
            .as_deref()
            .and_then(service_working_directory_from_text)
        {
            candidates.push(("systemd WorkingDirectory", dir));
        }
        candidates.push(("installer default", PathBuf::from(DEFAULT_EDGE_HOME)));
        candidates.push(("legacy installer default", PathBuf::from(LEGACY_EDGE_HOME)));
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(("working directory", cwd));
        }
        resolve_home(&candidates).map(Self::from_root)
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn runtime_root(&self) -> &Path {
        &self.root
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("configs")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    pub fn pki_dir(&self) -> PathBuf {
        self.root.join("pki")
    }

    /// Control-plane runtime override. It lives under `data/`, which the
    /// service user owns: `configs/` is root-controlled and the service only
    /// reads it, so writing the override there fails with EACCES.
    pub fn control_plane_runtime_file(&self) -> PathBuf {
        self.data_dir().join("control-plane-runtime.json")
    }

    pub fn api_config_file(&self) -> PathBuf {
        self.config_dir().join("api_node.yaml")
    }

    pub fn legacy_api_config_file(&self) -> PathBuf {
        self.root.join("api_node.yaml")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.data_dir().join("zelvik-edge.pid")
    }

    pub fn run_log_file(&self) -> PathBuf {
        self.logs_dir().join("run.log")
    }

    pub fn state_file(&self) -> PathBuf {
        self.data_dir().join("state.json")
    }

    pub fn metrics_db_dir(&self) -> PathBuf {
        self.data_dir().join("metrics.mace")
    }

    /// Mace must never be opened on the legacy RocksDB directory.
    pub fn mace_metrics_db_dir(&self) -> PathBuf {
        self.data_dir().join("metrics.mace")
    }

    pub fn xdp_state_file(&self) -> PathBuf {
        self.data_dir().join("xdp-state.json")
    }

    pub fn xdp_tuning_file(&self) -> PathBuf {
        self.data_dir().join("xdp-netdev-tuning.json")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir().join("cache")
    }

    pub fn geo_root(&self) -> PathBuf {
        self.data_dir().join("geo")
    }

    pub fn legacy_pid_file(&self) -> PathBuf {
        self.root.join("../data/zelvik-edge.pid")
    }

    pub fn legacy_state_file(&self) -> PathBuf {
        self.root.join("../data/state.json")
    }

    pub fn legacy_metrics_db_dir(&self) -> PathBuf {
        self.root.join("../data/metrics.db")
    }

    pub fn legacy_mace_metrics_db_dir(&self) -> PathBuf {
        self.root.join("../data/metrics.mace")
    }

    pub fn legacy_cache_dir(&self) -> PathBuf {
        self.root.join("configs/cache/disk")
    }

    pub fn api_config_candidates(&self) -> Vec<PathBuf> {
        vec![
            self.api_config_file(),
            self.legacy_api_config_file(),
            self.root.join("../configs/api_node.yaml"),
            self.root.join("zelvik-edge/configs/api_node.yaml"),
        ]
    }

    pub fn pid_file_candidates(&self) -> Vec<PathBuf> {
        vec![self.pid_file(), self.legacy_pid_file()]
    }

    pub fn state_file_candidates(&self) -> Vec<PathBuf> {
        vec![self.state_file(), self.legacy_state_file()]
    }

    pub fn metrics_db_candidates(&self) -> Vec<PathBuf> {
        vec![self.metrics_db_dir(), self.legacy_metrics_db_dir()]
    }

    pub fn mace_metrics_db_candidates(&self) -> Vec<PathBuf> {
        vec![
            self.mace_metrics_db_dir(),
            self.legacy_mace_metrics_db_dir(),
        ]
    }

    /// Create the directories the service user writes to. Errors are returned,
    /// never dropped: a node that cannot create `data/` cannot write its pid
    /// file, cache, geo store or control-plane override.
    pub fn ensure_runtime_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.config_dir())?;
        std::fs::create_dir_all(self.data_dir())?;
        std::fs::create_dir_all(self.logs_dir())?;
        Ok(())
    }
}

/// Reject paths that cannot name a component home even before installation.
pub fn is_safe_home_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return false;
    }
    if !path.is_absolute() {
        return false;
    }
    path.to_str().is_some_and(|value| {
        // Conservative whitelist: these characters can be interpolated into a
        // systemd unit and a shell wrapper without quoting traps.
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
            && !value.contains("/../")
            && !value.ends_with("/..")
    })
}

/// Accept a directory as a node home only when it can hold a real installation.
pub fn is_usable_home(path: &Path) -> bool {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        return false;
    }
    if !path.is_dir() {
        return false;
    }
    path.join("configs").is_dir() || path.join("data").is_dir()
}

/// Pick the first usable candidate and, when none works, report all of them.
pub fn resolve_home(candidates: &[(&str, PathBuf)]) -> anyhow::Result<PathBuf> {
    for (_source, path) in candidates {
        if is_usable_home(path) {
            return Ok(path.clone());
        }
    }
    let mut message = String::from(
        "no usable Zelvik-Edge home found: every candidate is missing, is '/', \
         or has neither configs/ nor data/",
    );
    for (source, path) in candidates {
        message.push_str(&format!("\n  {source}: {}", path.display()));
    }
    anyhow::bail!(message)
}

/// Installed unit text, read from disk. `systemctl cat` is not used here: the
/// node may run without a systemd bus (containers, tests), while the file is
/// the authority the installer wrote.
fn systemd_unit_text() -> Option<String> {
    static UNIT: OnceLock<Option<String>> = OnceLock::new();
    UNIT.get_or_init(|| std::fs::read_to_string(SYSTEMD_UNIT_PATH).ok())
        .clone()
}

fn normalize_config_path(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let unquoted = if trimmed.len() >= 2 {
        let first = trimmed.as_bytes()[0] as char;
        let last = trimmed.as_bytes()[trimmed.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    let unquoted = unquoted.trim();
    (!unquoted.is_empty()).then(|| PathBuf::from(unquoted))
}

fn split_config_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(quote_char) = quote {
            if ch == quote_char {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }

    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Extract `Environment=ZELVIK_EDGE_HOME=...` from unit text.
pub fn service_env_zelvik_edge_home_from_text(service: &str) -> Option<PathBuf> {
    for line in service.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("Environment=") else {
            continue;
        };
        for word in split_config_words(rest) {
            if let Some(value) = word.strip_prefix("ZELVIK_EDGE_HOME=") {
                return normalize_config_path(value);
            }
        }
    }
    None
}

/// Extract `WorkingDirectory=...` from unit text.
pub fn service_working_directory_from_text(service: &str) -> Option<PathBuf> {
    for line in service.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("WorkingDirectory=") else {
            continue;
        };
        if let Some(first_word) = split_config_words(rest).into_iter().next() {
            return normalize_config_path(&first_word);
        }
        return normalize_config_path(rest);
    }
    None
}

pub fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|path| path.exists()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_service_environment_and_working_directory() {
        let service = r#"
[Service]
Environment="FOO=bar" "ZELVIK_EDGE_HOME=/opt/zelvik/edge" BAZ=qux
WorkingDirectory='/opt/zelvik/edge'
"#;
        assert_eq!(
            service_env_zelvik_edge_home_from_text(service),
            Some(PathBuf::from("/opt/zelvik/edge"))
        );
        assert_eq!(
            service_working_directory_from_text(service),
            Some(PathBuf::from("/opt/zelvik/edge"))
        );
    }

    #[test]
    fn rejects_root_and_directories_without_layout() {
        assert!(!is_usable_home(Path::new("/")));
        assert!(!is_usable_home(Path::new("")));
        let base = std::env::temp_dir().join(format!(
            "zelvik-edge-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let empty = base.join("empty");
        let installed = base.join("installed");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(installed.join("configs")).unwrap();

        assert!(!is_usable_home(&empty));
        assert!(is_usable_home(&installed));

        assert!(is_safe_home_path(Path::new("/opt/zelvik/edge")));
        assert!(!is_safe_home_path(Path::new("/")));
        assert!(!is_safe_home_path(Path::new("relative/path")));
        assert!(!is_safe_home_path(Path::new("/opt/zelvik edge")));
        assert!(!is_safe_home_path(Path::new("/opt/zelvik$edge")));
        assert!(!is_safe_home_path(Path::new("/opt/../etc")));

        let candidates = vec![
            ("ZELVIK_EDGE_HOME", PathBuf::from("/")),
            ("working directory", empty.clone()),
            ("installer default", installed.clone()),
        ];
        assert_eq!(resolve_home(&candidates).unwrap(), installed);

        let rejected = vec![
            ("ZELVIK_EDGE_HOME", PathBuf::from("/")),
            ("working directory", empty),
        ];
        let err = resolve_home(&rejected).unwrap_err().to_string();
        assert!(err.contains("ZELVIK_EDGE_HOME"), "{err}");
        assert!(err.contains("working directory"), "{err}");

        std::fs::remove_dir_all(&base).unwrap();
    }
}
