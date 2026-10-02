//! Task-driven node self-upgrade: resolves the target package on the admin
//! management address, downloads it over plain HTTP(S) (never over the gRPC
//! control channel), verifies SHA-256, swaps the running binary atomically and
//! restarts the service. The CLI `upgrade` subcommand in `main.rs` shares the
//! asset-selection and archive primitives below.

use crate::api_config::ApiConfig;
use std::fs;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

pub const XDP_EBPF_OBJECT_NAME: &str = "cloudsidra-edge-xdp-ebpf.o";
const DEFAULT_BACKUP_DIR: &str = "/var/backups/cloudsidra/edge-upgrade";

#[derive(Debug)]
pub struct ExtractedReleasePayload {
    pub binary: PathBuf,
    pub xdp_ebpf_object: Option<PathBuf>,
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn cpuinfo_has_flag(cpuinfo: &str, flag: &str) -> bool {
    cpuinfo
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ':')
        .any(|part| part == flag)
}

pub fn select_release_asset(
    arch: &str,
    cpuinfo: &str,
    glibc_older_than_228: bool,
) -> anyhow::Result<&'static str> {
    match arch {
        "x86_64" | "amd64" => {
            if glibc_older_than_228 {
                anyhow::bail!(
                    "x86_64 systems with glibc older than 2.28 are not supported by official release assets"
                );
            }
            if !cpuinfo_has_flag(cpuinfo, "sse4_2") {
                anyhow::bail!(
                    "x86_64 CPU without SSE4.2 is not supported by official release assets"
                );
            }
            if cpuinfo_has_flag(cpuinfo, "avx512f") {
                Ok("CloudSidra-Edge-linux-x64-v4-avx512.tar.gz")
            } else if cpuinfo_has_flag(cpuinfo, "avx2") {
                Ok("CloudSidra-Edge-linux-x64-v3-avx2.tar.gz")
            } else {
                Ok("CloudSidra-Edge-linux-x64-v2-sse4.2.tar.gz")
            }
        }
        "aarch64" | "arm64" => {
            if cpuinfo.to_ascii_lowercase().contains("neoverse-n1") {
                Ok("CloudSidra-Edge-linux-arm64-neoverse-n1.tar.gz")
            } else {
                Ok("CloudSidra-Edge-linux-arm64-generic.tar.gz")
            }
        }
        other => anyhow::bail!("unsupported architecture for official release assets: {other}"),
    }
}

#[cfg(target_os = "linux")]
fn glibc_is_older_than_228() -> bool {
    let Ok(output) = Command::new("ldd").arg("--version").output() else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(first_line) = stdout.lines().next() else {
        return false;
    };
    let Some(version) = first_line.split_whitespace().find(|part| {
        part.chars().next().is_some_and(|ch| ch.is_ascii_digit()) && part.contains('.')
    }) else {
        return false;
    };
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(999);
    let minor = parts
        .next()
        .and_then(|value| {
            value
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(999);
    major < 2 || (major == 2 && minor < 28)
}

#[cfg(not(target_os = "linux"))]
fn glibc_is_older_than_228() -> bool {
    false
}

pub fn detect_release_asset() -> anyhow::Result<&'static str> {
    let arch = std::env::consts::ARCH;
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    select_release_asset(arch, &cpuinfo, glibc_is_older_than_228())
}

pub async fn download_to_file(url: &str, target: &Path) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .user_agent(format!("cloudsidra-edge/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client.get(url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("download failed with HTTP status {}", response.status());
    }

    let mut file = tokio::fs::File::create(target).await?;
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

fn archive_path_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

pub fn extract_release_payload(
    archive_path: &Path,
    target_dir: &Path,
) -> anyhow::Result<ExtractedReleasePayload> {
    let archive_file = fs::File::open(archive_path)?;
    let gz = flate2::read::GzDecoder::new(archive_file);
    let mut archive = tar::Archive::new(gz);
    let binary = target_dir.join("cloudsidra-edge");
    let xdp_ebpf_object = target_dir.join(XDP_EBPF_OBJECT_NAME);
    let mut found_binary = false;
    let mut found_xdp_ebpf_object = false;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let parts = archive_path_parts(&path);
        if parts == ["cloudsidra-edge"] {
            entry.unpack(&binary)?;
            if !binary.is_file() {
                anyhow::bail!("release archive cloudsidra-edge entry is not a regular file");
            }
            found_binary = true;
        } else if parts == ["data", XDP_EBPF_OBJECT_NAME] {
            entry.unpack(&xdp_ebpf_object)?;
            if !xdp_ebpf_object.is_file() {
                anyhow::bail!("release archive XDP eBPF object entry is not a regular file");
            }
            found_xdp_ebpf_object = true;
        }
    }

    if !found_binary {
        anyhow::bail!("release archive does not contain cloudsidra-edge");
    }

    Ok(ExtractedReleasePayload {
        binary,
        xdp_ebpf_object: found_xdp_ebpf_object.then_some(xdp_ebpf_object),
    })
}

pub fn backup_current_binary(
    install_binary: &Path,
    backup_dir: &Path,
    version: &str,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(backup_dir)?;
    let file_name = install_binary
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cloudsidra-edge");
    let backup_path = backup_dir.join(format!("{file_name}.{version}.{}.backup", unix_timestamp()));
    fs::copy(install_binary, &backup_path)?;
    Ok(backup_path)
}

pub fn replace_binary(source: &Path, install_binary: &Path) -> anyhow::Result<()> {
    let parent = install_binary
        .parent()
        .ok_or_else(|| anyhow::anyhow!("install binary has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let new_path = install_binary.with_extension("upgrade-new");
    fs::copy(source, &new_path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&new_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&new_path, perms)?;
    }

    fs::rename(&new_path, install_binary)?;
    Ok(())
}

pub fn install_xdp_ebpf_object_to_runtime(
    source: &Path,
    runtime_root: &Path,
) -> anyhow::Result<PathBuf> {
    let data_dir = runtime_root.join("data");
    fs::create_dir_all(&data_dir)?;
    let dest = data_dir.join(XDP_EBPF_OBJECT_NAME);
    let new_path = data_dir.join(format!("{XDP_EBPF_OBJECT_NAME}.upgrade-new"));
    fs::copy(source, &new_path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&new_path)?.permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&new_path, perms)?;
    }

    fs::rename(&new_path, &dest)?;
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Admin-driven task upgrade
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct PackageMeta {
    component: String,
    version: String,
    filename: String,
    sha256: String,
    #[serde(rename = "sizeBytes")]
    size_bytes: u64,
    url: String,
}

/// Parse an `upgradeNode` task type: `"upgradeNode"` or
/// `"upgradeNode@{"version":"26.10.0"}". Returns the explicit target version
/// when embedded (empty ⇒ latest synced package on the admin node).
pub fn parse_upgrade_task_version(task_type: &str) -> Option<Option<String>> {
    if task_type == "upgradeNode" {
        return Some(None);
    }
    if let Some(json) = task_type.strip_prefix("upgradeNode@")
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(json)
        && let Some(version) = value.get("version").and_then(|v| v.as_str())
        && !version.is_empty()
    {
        return Some(Some(version.to_string()));
    }
    None
}

/// Outcome of [`execute_node_upgrade`]. `true` means a new binary was
/// installed and the service must be restarted to load it.
pub async fn execute_node_upgrade(
    api_config: &ApiConfig,
    task_type: &str,
) -> anyhow::Result<bool> {
    let Some(target_version) = parse_upgrade_task_version(task_type) else {
        anyhow::bail!("not an upgradeNode task: {task_type}");
    };
    if !cfg!(target_os = "linux") {
        anyhow::bail!("node task upgrade is currently supported only on Linux release assets");
    }

    let asset = detect_release_asset()?;
    let endpoints = api_config.effective_update_endpoints();
    if endpoints.is_empty() {
        anyhow::bail!("no update endpoints configured (update.endpoints or rpc.endpoints)");
    }

    // Resolve which package the admin node wants us to install.
    let mut last_err: Option<anyhow::Error> = None;
    let mut resolved: Option<(String, PackageMeta)> = None;
    for base in &endpoints {
        match fetch_package_meta(base, target_version.as_deref(), asset).await {
            Ok(meta) => {
                resolved = Some((base.clone(), meta));
                break;
            }
            Err(err) => {
                warn!("package meta lookup failed via {base}: {err}");
                last_err = Some(err);
            }
        }
    }
    let (base, meta) = resolved
        .ok_or_else(|| last_err.unwrap_or_else(|| anyhow::anyhow!("no usable update endpoint")))?;

    if meta.version == env!("CARGO_PKG_VERSION") {
        info!(
            "node already runs {} ({}); upgrade task is a no-op",
            meta.version, meta.filename
        );
        return Ok(false);
    }

    info!(
        "node upgrade: {} -> {} via {} ({} bytes, sha256 {})",
        env!("CARGO_PKG_VERSION"),
        meta.version,
        base,
        meta.size_bytes,
        meta.sha256
    );

    let tmp_dir = std::env::temp_dir().join(format!(
        "cloudsidra-edge-task-upgrade-{}-{}",
        std::process::id(),
        unix_timestamp()
    ));
    fs::create_dir_all(&tmp_dir)?;
    let archive_path = tmp_dir.join(&meta.filename);

    let result = async {
        let download_url = format!("{}{}", base.trim_end_matches('/'), meta.url);
        download_to_file(&download_url, &archive_path).await?;

        // Verify integrity before touching the install location.
        let bytes = fs::read(&archive_path)?;
        use sha2::Digest;
        let sum = hex::encode(sha2::Sha256::digest(&bytes));
        if !sum.eq_ignore_ascii_case(&meta.sha256) {
            anyhow::bail!("package sha256 mismatch: got {sum} want {}", meta.sha256);
        }
        let payload = extract_release_payload(&archive_path, &tmp_dir)?;

        let install_binary = std::env::current_exe()?.canonicalize()?;
        let backup_dir = PathBuf::from(DEFAULT_BACKUP_DIR);
        let backup = backup_current_binary(&install_binary, &backup_dir, &meta.version)?;
        info!("current binary backed up to {}", backup.display());

        replace_binary(&payload.binary, &install_binary)?;
        info!("installed new binary at {}", install_binary.display());

        if let Some(object) = payload.xdp_ebpf_object.as_deref() {
            let runtime_root = crate::paths::NodePaths::current().runtime_root().to_path_buf();
            match install_xdp_ebpf_object_to_runtime(object, &runtime_root) {
                Ok(dest) => info!("installed XDP eBPF object to {}", dest.display()),
                Err(err) => warn!("XDP eBPF object install failed: {err}"),
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = fs::remove_dir_all(&tmp_dir);
    result?;

    Ok(true)
}

async fn fetch_package_meta(
    base: &str,
    target_version: Option<&str>,
    asset: &str,
) -> anyhow::Result<PackageMeta> {
    let mut url = format!(
        "{}/v1/edge/packages/meta?component=CloudSidra-Edge&asset={}",
        base.trim_end_matches('/'),
        urlencoding(asset)
    );
    if let Some(version) = target_version {
        url.push_str(&format!("&version={}", urlencoding(version)));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(format!("cloudsidra-edge/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let response = client.get(&url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("package meta returned HTTP {}", response.status());
    }
    let meta = serde_json::from_str::<PackageMeta>(&response.text().await?)?;
    if meta.component != "CloudSidra-Edge" || meta.filename != asset {
        anyhow::bail!("package meta returned unexpected asset {}", meta.filename);
    }
    if meta.sha256.is_empty() || !meta.url.starts_with('/') {
        anyhow::bail!("package meta missing sha256 or relative url");
    }
    Ok(meta)
}

fn urlencoding(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => ch.to_string(),
            _ => format!("%{:02X}", ch as u32),
        })
        .collect()
}

/// Restart the running service so the swapped binary takes over. The request
/// is detached: under systemd, PID1 stops this unit and starts the new binary;
/// without systemd the spawned `restart` subcommand stops this process and
/// daemonizes a fresh instance.
pub fn schedule_upgrade_restart() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let unit_exists = Command::new("systemctl")
            .args(["cat", "cloudsidra-edge.service"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if unit_exists {
            Command::new("systemctl")
                .args(["restart", "cloudsidra-edge.service"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            return Ok(());
        }
    }
    Command::new(std::env::current_exe()?)
        .arg("restart")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upgrade_task_versions() {
        assert_eq!(parse_upgrade_task_version("upgradeNode"), Some(None));
        assert_eq!(
            parse_upgrade_task_version("upgradeNode@{\"version\":\"26.10.0\"}"),
            Some(Some("26.10.0".to_string()))
        );
        assert_eq!(parse_upgrade_task_version("installNode"), None);
        assert_eq!(parse_upgrade_task_version("upgradeNode@bogus"), None);
        assert_eq!(
            parse_upgrade_task_version("upgradeNode@{\"other\":1}"),
            None
        );
    }
}
