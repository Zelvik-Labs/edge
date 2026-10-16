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

pub const XDP_EBPF_OBJECT_NAME: &str = "zelvik-edge-xdp-ebpf.o";
const DEFAULT_BACKUP_DIR: &str = "/var/backups/zelvik/edge-upgrade";

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

/// Path to the on-disk executable file. On Linux, once the running binary's
/// file has been replaced or removed (an earlier atomic swap, a package
/// manager replacing the file), /proc/self/exe reports "<path> (deleted)",
/// which neither canonicalize() nor Command::new() can use. Strip the marker
/// so callers operate on the install path where the new binary now lives.
fn current_exe_install_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    match exe.to_str().and_then(|p| p.strip_suffix(" (deleted)")) {
        Some(stripped) => Ok(PathBuf::from(stripped)),
        None => Ok(exe),
    }
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
                Ok("Zelvik-Edge-linux-x64-v4-avx512.tar.gz")
            } else if cpuinfo_has_flag(cpuinfo, "avx2") {
                Ok("Zelvik-Edge-linux-x64-v3-avx2.tar.gz")
            } else {
                Ok("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz")
            }
        }
        "aarch64" | "arm64" => {
            if cpuinfo.to_ascii_lowercase().contains("neoverse-n1") {
                Ok("Zelvik-Edge-linux-arm64-neoverse-n1.tar.gz")
            } else {
                Ok("Zelvik-Edge-linux-arm64-generic.tar.gz")
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

fn download_client(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("zelvik-edge/{}", crate::RELEASE_VERSION))
}

/// Credentials belong in headers, never in URLs (which are shown to operators).
pub fn update_origin(base: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(base)
        .map_err(|_| anyhow::anyhow!("invalid update base URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        anyhow::bail!("update base URL must be an HTTP(S) origin without credentials, path, query or fragment");
    }
    Ok(url)
}

pub fn update_key_headers(key: &str) -> anyhow::Result<reqwest::header::HeaderMap> {
    let Some(hex) = key.strip_prefix("zvu_") else {
        anyhow::bail!("update key must be zvu_ followed by 64 hexadecimal characters");
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        anyhow::bail!("update key must be zvu_ followed by 64 hexadecimal characters");
    }
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))?;
    value.set_sensitive(true);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, value);
    Ok(headers)
}

/// Direct license-hub download; node task downloads use instance authentication.
pub async fn download_release_to_file(
    url: &reqwest::Url,
    target: &Path,
    headers: reqwest::header::HeaderMap,
) -> anyhow::Result<()> {
    if url.scheme() != "https" {
        anyhow::bail!("license package downloads require HTTPS");
    }
    let client = download_client(Duration::from_secs(600)).build()?;
    download_to_file(&client, url.clone(), target, headers).await
}

async fn download_to_file(
    client: &reqwest::Client,
    url: reqwest::Url,
    target: &Path,
    headers: reqwest::header::HeaderMap,
) -> anyhow::Result<()> {
    let mut response = client.get(url).headers(headers).send().await?;
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
    let binary = target_dir.join("zelvik-edge");
    let xdp_ebpf_object = target_dir.join(XDP_EBPF_OBJECT_NAME);
    let mut found_binary = false;
    let mut found_xdp_ebpf_object = false;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let parts = archive_path_parts(&path);
        if parts == ["zelvik-edge"] {
            if found_binary || !entry.header().entry_type().is_file() {
                anyhow::bail!("release binary must be a unique regular file");
            }
            entry.unpack(&binary)?;
            if !binary.is_file() {
                anyhow::bail!("release archive zelvik-edge entry is not a regular file");
            }
            found_binary = true;
        } else if parts == ["data", XDP_EBPF_OBJECT_NAME] {
            if found_xdp_ebpf_object || !entry.header().entry_type().is_file() {
                anyhow::bail!("release XDP object must be a unique regular file");
            }
            entry.unpack(&xdp_ebpf_object)?;
            if !xdp_ebpf_object.is_file() {
                anyhow::bail!("release archive XDP eBPF object entry is not a regular file");
            }
            found_xdp_ebpf_object = true;
        }
    }

    if !found_binary {
        anyhow::bail!("release archive does not contain zelvik-edge");
    }

    Ok(ExtractedReleasePayload {
        binary,
        xdp_ebpf_object: found_xdp_ebpf_object.then_some(xdp_ebpf_object),
    })
}

/// Reject malformed/wrong-machine payloads and probe the program before installation.
pub fn validate_release_payload(
    payload: &ExtractedReleasePayload,
    version: &str,
) -> anyhow::Result<()> {
    use std::io::Read;
    fn elf(path: &Path, machine: u16, types: &[u16]) -> anyhow::Result<()> {
        let mut header = [0u8; 64];
        fs::File::open(path)?.read_exact(&mut header)?;
        if &header[..4] != b"\x7fELF"
            || header[4] != 2
            || header[5] != 1
            || header[6] != 1
            || u16::from_le_bytes([header[18], header[19]]) != machine
            || !types.contains(&u16::from_le_bytes([header[16], header[17]]))
        {
            anyhow::bail!("invalid ELF format or architecture: {}", path.display());
        }
        Ok(())
    }
    let machine = match std::env::consts::ARCH {
        "x86_64" => 62,
        "aarch64" => 183,
        other => anyhow::bail!("unsupported upgrade architecture: {other}"),
    };
    elf(&payload.binary, machine, &[2, 3])?;
    let object = payload
        .xdp_ebpf_object
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("release archive is missing the XDP eBPF object"))?;
    elf(object, 247, &[1])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&payload.binary, fs::Permissions::from_mode(0o755))?;
    }
    let mut probe = Command::new(&payload.binary)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if probe.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            probe.kill()?;
            probe.wait()?;
            anyhow::bail!("release binary version probe timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = probe.wait_with_output()?;
    let text = String::from_utf8(output.stdout)?;
    let reported = text
        .trim()
        .strip_prefix("zelvik-edge ")
        .ok_or_else(|| {
            anyhow::anyhow!("release binary did not report a Zelvik-Edge version")
        })?;
    if !output.status.success()
        || (version != "latest" && reported != version.trim_start_matches('v'))
    {
        anyhow::bail!("release version probe failed: requested {version}, reported {reported}");
    }
    Ok(())
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct UpgradeInstallation {
    binary: PathBuf,
    binary_backup: PathBuf,
    object: PathBuf,
    object_backup: Option<PathBuf>,
    pub manifest: PathBuf,
    #[serde(default)]
    task_id: Option<i64>,
}

impl UpgradeInstallation {
    pub fn rollback(&self) -> anyhow::Result<()> {
        // Restore both members before restarting the old executable.
        if let Some(backup) = &self.object_backup {
            let root = self
                .object
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| anyhow::anyhow!("invalid object destination"))?;
            install_xdp_ebpf_object_to_runtime(backup, root)?;
        } else if self.object.exists() {
            fs::remove_file(&self.object)?;
        }
        replace_binary(&self.binary_backup, &self.binary)?;
        fs::write(self.manifest.with_extension("result"), b"rolled-back\n")?;
        Ok(())
    }
    pub fn restart_with_rollback(&self) -> anyhow::Result<()> {
        let restart = || -> anyhow::Result<()> {
            let status = Command::new("systemctl")
                .args(["restart", "zelvik-edge.service"])
                .status()?;
            if !status.success() {
                anyhow::bail!("service restart failed: {status}");
            }
            // Startup failures may occur after systemctl returns. Require stability.
            for _ in 0..15 {
                std::thread::sleep(Duration::from_secs(1));
                if !Command::new("systemctl")
                    .args(["is-active", "--quiet", "zelvik-edge.service"])
                    .status()?
                    .success()
                {
                    anyhow::bail!("upgraded service did not remain active");
                }
            }
            Ok(())
        };
        if let Err(error) = restart() {
            let stopped = Command::new("systemctl")
                .args(["stop", "zelvik-edge.service"])
                .status()?;
            if !stopped.success() {
                anyhow::bail!(
                    "upgrade failed ({error}); service stop failed, manual recovery required"
                );
            }
            self.rollback()?;
            restart().map_err(|restore| {
                anyhow::anyhow!("upgrade failed ({error}); rollback restart failed: {restore}")
            })?;
            anyhow::bail!("upgrade failed and previous binary/eBPF restored: {error}");
        }
        fs::write(self.manifest.with_extension("result"), b"process-stable\n")?;
        Ok(())
    }
}

pub fn install_release_payload(
    payload: &ExtractedReleasePayload,
    binary: &Path,
    root: &Path,
    backup_dir: &Path,
    version: &str,
) -> anyhow::Result<UpgradeInstallation> {
    validate_release_payload(payload, version)?;
    let dir = backup_dir.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    let binary_backup = backup_current_binary(binary, &dir, version)?;
    let object = root.join("data").join(XDP_EBPF_OBJECT_NAME);
    let object_backup = if object.exists() {
        let backup = dir.join(XDP_EBPF_OBJECT_NAME);
        fs::copy(&object, &backup)?;
        Some(backup)
    } else {
        None
    };
    let state = UpgradeInstallation {
        binary: binary.to_path_buf(),
        binary_backup,
        object,
        object_backup,
        manifest: dir.join("upgrade.json"),
        task_id: None,
    };
    fs::write(&state.manifest, serde_json::to_vec(&state)?)?;
    let result = (|| -> anyhow::Result<()> {
        install_xdp_ebpf_object_to_runtime(
            payload.xdp_ebpf_object.as_ref().expect("validated payload"),
            root,
        )?;
        replace_binary(&payload.binary, binary)?;
        Ok(())
    })();
    if let Err(error) = result {
        state.rollback().map_err(|restore| {
            anyhow::anyhow!("install failed ({error}); rollback failed: {restore}")
        })?;
        return Err(error);
    }
    Ok(state)
}

pub fn run_upgrade_guard(manifest: &Path) -> anyhow::Result<()> {
    let state: UpgradeInstallation = serde_json::from_slice(&fs::read(manifest)?)?;
    let result = state.restart_with_rollback();
    if let Some(task_id) = state.task_id {
        let config_path = ApiConfig::default_paths()
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| {
                anyhow::anyhow!("upgrade task result not reported: API config missing")
            })?;
        let config = ApiConfig::load(config_path)?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let client = crate::rpc::client::RpcClient::new(&config).await?;
                let mut service = client.node_task_service();
                service
                    .report_node_task_done(crate::pb::ReportNodeTaskDoneRequest {
                        node_task_id: task_id,
                        is_ok: result.is_ok(),
                        error: result
                            .as_ref()
                            .err()
                            .map(|e| format!("{e:#}"))
                            .unwrap_or_default(),
                    })
                    .await?;
                Ok::<(), anyhow::Error>(())
            })?;
    }
    result
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
        .unwrap_or("zelvik-edge");
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

struct NodeUpdateClient {
    client: reqwest::Client,
    origin: reqwest::Url,
    server_name: Option<String>,
}

impl NodeUpdateClient {
    async fn new(api_config: &ApiConfig, base: &str) -> anyhow::Result<Self> {
        let origin = update_origin(base)?;
        let mode = api_config.comm_mode()?;
        if mode != zelvik_suite::CommMode::Plaintext && origin.scheme() != "https" {
            anyhow::bail!("TLS node update transport requires an HTTPS endpoint");
        }
        let mut builder = download_client(Duration::from_secs(600));
        let mut server_name = None;
        if mode != zelvik_suite::CommMode::Plaintext {
            builder = builder.tls_version_min(reqwest::tls::Version::TLS_1_3);
            if let Some(path) = &api_config.rpc_tls.ca_file {
                let certs = reqwest::Certificate::from_pem_bundle(&fs::read(path)?)?;
                if certs.is_empty() {
                    anyhow::bail!("node update CA bundle is empty");
                }
                builder = builder.tls_certs_merge(certs);
            }
            if mode == zelvik_suite::CommMode::Mtls {
                let cert = api_config.rpc_tls.cert_file.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("mTLS node updates require rpc.tls.certFile")
                })?;
                let key = api_config.rpc_tls.key_file.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("mTLS node updates require rpc.tls.keyFile")
                })?;
                let mut pem = fs::read(cert)?;
                pem.push(b'\n');
                pem.extend(fs::read(key)?);
                builder = builder.identity(reqwest::Identity::from_pem(&pem)?);
            }
            if let Some(name) = &api_config.rpc_tls.server_name
                && Some(name.as_str()) != origin.host_str()
            {
                // Use the configured TLS name for verification/SNI while connecting
                // to the update endpoint and preserving its HTTP authority.
                let mut tls_url = origin.clone();
                tls_url.set_host(Some(name))?;
                let addresses = tokio::net::lookup_host((
                    origin.host_str().ok_or_else(|| anyhow::anyhow!("missing update host"))?,
                    origin.port_or_known_default().ok_or_else(|| anyhow::anyhow!("missing update port"))?,
                )).await?.collect::<Vec<_>>();
                if addresses.is_empty() {
                    anyhow::bail!("node update endpoint resolved to no addresses");
                }
                builder = builder.resolve_to_addrs(name, &addresses);
                server_name = Some(name.clone());
            }
        }
        Ok(Self { client: builder.build()?, origin, server_name })
    }

    fn request_url(&self, mut url: reqwest::Url) -> anyhow::Result<reqwest::Url> {
        if url.origin() != self.origin.origin() {
            anyhow::bail!("node update URL must stay on the configured origin");
        }
        if let Some(name) = &self.server_name {
            url.set_host(Some(name))?;
        }
        Ok(url)
    }

    fn headers(&self, api_config: &ApiConfig) -> anyhow::Result<reqwest::header::HeaderMap> {
        use tonic::service::Interceptor;
        let request = api_config.credential_interceptor()?.call(tonic::Request::new(()))?;
        let mut headers = reqwest::header::HeaderMap::new();
        for key in [
            zelvik_suite::METADATA_INSTANCE,
            zelvik_suite::METADATA_CREDENTIAL,
            zelvik_suite::METADATA_KIND,
            zelvik_suite::METADATA_PROTOCOL,
        ] {
            let metadata = request.metadata().get(key)
                .ok_or_else(|| anyhow::anyhow!("missing node update identity header"))?;
            let mut value = reqwest::header::HeaderValue::from_bytes(metadata.as_encoded_bytes())?;
            value.set_sensitive(true);
            headers.insert(key, value);
        }
        if self.server_name.is_some() {
            headers.insert(reqwest::header::HOST, self.origin.authority().parse()?);
        }
        Ok(headers)
    }
}

fn package_download_url(origin: &reqwest::Url, path: &str) -> anyhow::Result<reqwest::Url> {
    if !path.starts_with("/v1/edge/packages/download/") || path.contains('\\') {
        anyhow::bail!("package metadata returned an invalid download path");
    }
    let url = origin.join(path)?;
    if url.origin() != origin.origin() || url.fragment().is_some()
        || !url.path().starts_with("/v1/edge/packages/download/")
    {
        anyhow::bail!("package metadata download URL must stay on the configured origin");
    }
    Ok(url)
}

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
    task_id: i64,
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
    let mut resolved: Option<(NodeUpdateClient, PackageMeta)> = None;
    for base in &endpoints {
        let lookup = async {
            let update = NodeUpdateClient::new(api_config, base).await?;
            let meta = fetch_package_meta(&update, api_config, target_version.as_deref(), asset).await?;
            Ok::<_, anyhow::Error>((update, meta))
        }.await;
        match lookup {
            Ok(value) => {
                resolved = Some(value);
                break;
            }
            Err(err) => {
                warn!("package meta lookup failed via {base}: {err}");
                last_err = Some(err);
            }
        }
    }
    let (update, meta) = resolved
        .ok_or_else(|| last_err.unwrap_or_else(|| anyhow::anyhow!("no usable update endpoint")))?;

    if meta.version.trim_start_matches('v') == crate::RELEASE_VERSION {
        info!(
            "node already runs {} ({}); upgrade task is a no-op",
            meta.version, meta.filename
        );
        return Ok(false);
    }

    info!(
        "node upgrade: {} -> {} via {} ({} bytes, sha256 {})",
        crate::RELEASE_VERSION,
        meta.version,
        update.origin,
        meta.size_bytes,
        meta.sha256
    );

    let tmp_dir = std::env::temp_dir().join(format!(
        "zelvik-edge-task-upgrade-{}-{}",
        std::process::id(),
        unix_timestamp()
    ));
    fs::create_dir_all(&tmp_dir)?;
    let archive_path = tmp_dir.join(&meta.filename);

    let result = async {
        let download_url = package_download_url(&update.origin, &meta.url)?;
        download_to_file(
            &update.client,
            update.request_url(download_url)?,
            &archive_path,
            update.headers(api_config)?,
        ).await?;

        // Verify integrity before touching the install location.
        let bytes = fs::read(&archive_path)?;
        use sha2::Digest;
        let sum = hex::encode(sha2::Sha256::digest(&bytes));
        if !sum.eq_ignore_ascii_case(&meta.sha256) {
            anyhow::bail!("package sha256 mismatch: got {sum} want {}", meta.sha256);
        }
        let payload = extract_release_payload(&archive_path, &tmp_dir)?;

        let install_binary = current_exe_install_path()?.canonicalize()?;
        let root = crate::paths::NodePaths::current()
            .runtime_root()
            .to_path_buf();
        let mut installation = install_release_payload(
            &payload,
            &install_binary,
            &root,
            Path::new(DEFAULT_BACKUP_DIR),
            &meta.version,
        )?;
        installation.task_id = Some(task_id);
        fs::write(&installation.manifest, serde_json::to_vec(&installation)?)?;
        if let Err(error) = schedule_upgrade_restart(&installation.manifest) {
            installation.rollback()?;
            return Err(error);
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let _ = fs::remove_dir_all(&tmp_dir);
    result?;

    Ok(true)
}

async fn fetch_package_meta(
    update: &NodeUpdateClient,
    api_config: &ApiConfig,
    target_version: Option<&str>,
    asset: &str,
) -> anyhow::Result<PackageMeta> {
    let mut url = update.origin.join("/v1/edge/packages/meta")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("component", "Zelvik-Edge");
        query.append_pair("asset", asset);
        if let Some(version) = target_version {
            query.append_pair("version", version);
        }
    }
    let response = update.client.get(update.request_url(url)?)
        .timeout(Duration::from_secs(30))
        .headers(update.headers(api_config)?)
        .send().await?;
    if !response.status().is_success() {
        anyhow::bail!("package meta returned HTTP {}", response.status());
    }
    let meta = serde_json::from_str::<PackageMeta>(&response.text().await?)?;
    if meta.component != "Zelvik-Edge" || meta.filename != asset {
        anyhow::bail!("package meta returned unexpected asset {}", meta.filename);
    }
    if meta.sha256.is_empty() {
        anyhow::bail!("package meta missing sha256");
    }
    package_download_url(&update.origin, &meta.url)?;
    Ok(meta)
}

/// Restart the running service so the swapped binary takes over. The request
/// is detached: under systemd, PID1 stops this unit and starts the new binary;
/// without systemd the spawned `restart` subcommand stops this process and
/// daemonizes a fresh instance.
fn schedule_upgrade_restart(manifest: &Path) -> anyhow::Result<()> {
    // A transient unit survives the edge unit stopping and owns recovery.
    let state: UpgradeInstallation = serde_json::from_slice(&fs::read(manifest)?)?;
    let unit = format!("zelvik-edge-upgrade-{}", uuid::Uuid::new_v4());
    let status = Command::new("systemd-run")
        .args(["--collect", "--unit", &unit])
        .arg(format!(
            "--setenv=ZELVIK_EDGE_HOME={}",
            state
                .object
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| anyhow::anyhow!("invalid upgrade runtime root"))?
                .display()
        ))
        .arg(&state.binary_backup)
        .arg("upgrade-guard")
        .arg("--manifest")
        .arg(manifest)
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to schedule supervised upgrade: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_elf_before_installing() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("new");
        fs::write(&binary, b"not an executable").unwrap();
        let installed = dir.join("installed");
        fs::write(&installed, b"old binary").unwrap();
        let payload = ExtractedReleasePayload {
            binary,
            xdp_ebpf_object: None,
        };
        assert!(
            install_release_payload(&payload, &installed, &dir, &dir.join("backup"), "26.9.0")
                .is_err()
        );
        assert_eq!(fs::read(&installed).unwrap(), b"old binary");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rollback_restores_both_package_members() {
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(dir.join("data")).unwrap();
        let binary = dir.join("installed");
        let binary_backup = dir.join("binary.old");
        let object = dir.join("data").join(XDP_EBPF_OBJECT_NAME);
        let object_backup = dir.join("object.old");
        fs::write(&binary, b"new binary").unwrap();
        fs::write(&binary_backup, b"old binary").unwrap();
        fs::write(&object, b"new object").unwrap();
        fs::write(&object_backup, b"old object").unwrap();
        let state = UpgradeInstallation {
            binary: binary.clone(),
            binary_backup,
            object: object.clone(),
            object_backup: Some(object_backup),
            manifest: dir.join("upgrade.json"),
            task_id: None,
        };
        state.rollback().unwrap();
        assert_eq!(fs::read(binary).unwrap(), b"old binary");
        assert_eq!(fs::read(object).unwrap(), b"old object");
        fs::remove_dir_all(dir).unwrap();
    }

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
