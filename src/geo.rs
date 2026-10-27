//! Geo asset consumption: pull one bundle generation over the authenticated
//! AssetControl channel, check its manifest and component digests, activate it
//! atomically, and use it as the query-path source. Nodes need no separate Geo key.
//!
//! Activation order is load-bearing: components are digest-checked in memory,
//! written and fsynced to the staging area by [`GeoStore`], opened from the
//! staging path, and only then published by an atomic active-pointer switch.
//! The query path shares the same [`GeoStore`], so the ACK the control plane
//! receives says `active` only after lookups can actually use the generation.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use zelvik_geo_runtime::{
    BundleManifest, ComponentKind, GeoStore, GeoStoreError, StagedBundle,
};
use zelvik_suite::pb::asset::v1::{
    AcknowledgeAssetRequest, AssetDescriptor, AssetKind, AssetState, DownloadAssetRequest,
    GetAssetManifestRequest, asset_control_client::AssetControlClient,
};

use crate::api_config::ApiConfig;
use crate::rpc::client::SharedRpcClient;

/// One component the node must download for a bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetComponent {
    pub kind: ComponentKind,
    pub version: String,
    pub size: u64,
    pub sha256: String,
}

/// The asset manifest the control plane returned for one pull.
#[derive(Clone, Debug)]
pub struct AssetManifestResponse {
    /// Empty when the node is not assigned any bundle; it keeps what it has.
    pub bundle_id: String,
    pub generation: u64,
    pub manifest_json: Vec<u8>,
    pub update_required: bool,
    pub rollback_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetAckState {
    Active,
    Staged,
    Failed,
}

impl AssetAckState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Staged => "staged",
            Self::Failed => "failed",
        }
    }
}

/// One acknowledgement sent after an activation attempt.
#[derive(Clone, Debug)]
pub struct AssetAck {
    pub bundle_id: String,
    pub generation: u64,
    pub components: Vec<AssetComponent>,
    pub state: AssetAckState,
    pub error: String,
}

/// The control-plane asset surface the agent needs.
#[async_trait::async_trait]
pub trait AssetTransport: Send {
    async fn get_asset_manifest(&mut self, known_generation: u64) -> Result<AssetManifestResponse>;
    async fn download_asset(
        &mut self,
        bundle_id: &str,
        generation: u64,
        kind: &str,
    ) -> Result<Vec<u8>>;
    async fn acknowledge_asset(&mut self, ack: AssetAck) -> Result<()>;
}

/// What one reconciliation did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeoReconcileReport {
    pub activated: bool,
    pub bundle_id: String,
    pub generation: u64,
    pub unchanged: bool,
}

/// The geo agent. `run_once` is the whole reconciliation.
pub struct GeoAgent<T: AssetTransport> {
    transport: T,
    store: std::sync::Arc<GeoStore>,
}

impl<T: AssetTransport> GeoAgent<T> {
    /// Opens the store (creating its layout and cleaning staging leftovers) and
    /// installs it as the query-path source immediately. A corrupt active
    /// bundle is a startup error, not silently treated as "no geo data".
    pub fn open(
        transport: T,
        geo_root: impl Into<std::path::PathBuf>,
    ) -> Result<Self> {
        let store = std::sync::Arc::new(
            GeoStore::open(geo_root).map_err(|e| anyhow::anyhow!("open geo store: {e}"))?,
        );
        install_store(&store);
        Ok(Self {
            transport,
            store,
        })
    }

    pub fn active_generation(&self) -> u64 {
        self.store.generation()
    }

    pub fn active_bundle_id(&self) -> String {
        self.store.active_bundle_id()
    }

    /// The transport, for tests that drive the reconciliation from a fake.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// One reconciliation pass.
    pub async fn run_once(&mut self) -> Result<GeoReconcileReport> {
        let response = self
            .transport
            .get_asset_manifest(self.store.generation())
            .await
            .context("get asset manifest")?;
        if response.bundle_id.is_empty() {
            return Ok(GeoReconcileReport {
                unchanged: true,
                generation: self.store.generation(),
                ..Default::default()
            });
        }
        let manifest = match self.verify_manifest(&response) {
            Ok(manifest) => manifest,
            Err(err) => {
                // The response identifies the target even when its manifest is
                // invalid. A failed ACK reports no component or activation facts.
                if let Err(ack_err) = self
                    .ack(AssetAck {
                        bundle_id: response.bundle_id.clone(),
                        generation: response.generation,
                        components: Vec::new(),
                        state: AssetAckState::Failed,
                        error: err.to_string(),
                    })
                    .await
                {
                    warn!(
                        bundle_id = %response.bundle_id,
                        generation = response.generation,
                        error = %ack_err,
                        "failed asset acknowledgement could not be delivered"
                    );
                }
                return Err(err);
            }
        };
        let components = components_of(&manifest);

        // A previous pass may have activated the bundle but failed to deliver
        // its ACK (or the process restarted). The durable store already holds
        // the generation, so re-ack it instead of downloading hundreds of
        // megabytes again. The manifest above was still verified first.
        if manifest.generation == self.store.generation()
            && manifest.bundle_id == self.store.active_bundle_id()
        {
            if self.store.current().is_some_and(|snapshot| {
                snapshot.manifest().manifest_sha256 != manifest.manifest_sha256
            }) {
                bail!("installed geo bundle identity was reused with a different manifest");
            }
            self.ack(AssetAck {
                bundle_id: manifest.bundle_id,
                generation: manifest.generation,
                components,
                state: AssetAckState::Active,
                error: String::new(),
            })
            .await?;
            return Ok(GeoReconcileReport {
                activated: false,
                bundle_id: response.bundle_id,
                generation: response.generation,
                unchanged: true,
            });
        }

        match self.download_and_activate(&manifest).await {
            Ok(()) => {
                self.ack(AssetAck {
                    bundle_id: manifest.bundle_id.clone(),
                    generation: manifest.generation,
                    components,
                    state: AssetAckState::Active,
                    error: String::new(),
                })
                .await?;
                Ok(GeoReconcileReport {
                    activated: true,
                    bundle_id: manifest.bundle_id,
                    generation: manifest.generation,
                    unchanged: false,
                })
            }
            Err(err) => {
                // The previous generation keeps serving; report the failure.
                if let Err(ack_err) = self
                    .ack(AssetAck {
                        bundle_id: manifest.bundle_id.clone(),
                        generation: manifest.generation,
                        components,
                        state: AssetAckState::Failed,
                        error: err.to_string(),
                    })
                    .await
                {
                    warn!(
                        bundle_id = %manifest.bundle_id,
                        generation = manifest.generation,
                        error = %ack_err,
                        "failed asset acknowledgement could not be delivered"
                    );
                }
                Err(err)
            }
        }
    }

    async fn ack(&mut self, ack: AssetAck) -> Result<()> {
        self.transport
            .acknowledge_asset(ack)
            .await
            .context("acknowledge asset")
    }

    fn verify_manifest(&self, response: &AssetManifestResponse) -> Result<BundleManifest> {
        if response.manifest_json.is_empty() {
            bail!("asset manifest is empty");
        }
        let manifest: BundleManifest =
            serde_json::from_slice(&response.manifest_json).context("parse bundle manifest")?;
        if manifest.bundle_id != response.bundle_id || manifest.generation != response.generation {
            bail!("manifest identity disagrees with the response");
        }
        if !manifest.verify_digest() {
            bail!("bundle manifest digest mismatch");
        }
        manifest.validate().context("bundle manifest invalid")?;
        Ok(manifest)
    }

    async fn download_and_activate(&mut self, manifest: &BundleManifest) -> Result<()> {
        let mut components: BTreeMap<ComponentKind, Vec<u8>> = BTreeMap::new();
        for comp in &manifest.components {
            let kind = component_kind_str(comp.kind);
            let bytes = self
                .transport
                .download_asset(&manifest.bundle_id, manifest.generation, kind)
                .await
                .with_context(|| format!("download {kind} asset"))?;
            if bytes.len() as u64 != comp.size {
                bail!(
                    "{kind} asset size {} != manifest {}",
                    bytes.len(),
                    comp.size
                );
            }
            let digest = hex::encode(Sha256::digest(&bytes));
            if !digest.eq_ignore_ascii_case(&comp.sha256) {
                bail!("{kind} asset digest mismatch");
            }
            components.insert(comp.kind, bytes);
        }
        let staged = StagedBundle {
            manifest: manifest.clone(),
            components,
        };
        self.store
            .activate(staged)
            .map_err(|e: GeoStoreError| anyhow::anyhow!("activate bundle: {e}"))?;
        // The store is the same Arc the query path holds; this refreshes the
        // generation counters and drops cached lookups from the old bundle.
        install_store(&self.store);
        info!(
            bundle = %manifest.bundle_id,
            generation = manifest.generation,
            "geo bundle activated"
        );
        Ok(())
    }
}

/// Publish the store to the query path and account its bytes.
fn install_store(store: &std::sync::Arc<GeoStore>) {
    // The unified MMDB is the only location file; it already contains City,
    // Country and ASN. Count its bytes once rather than pretending a separate
    // ASN database exists.
    let (unified_bytes, asn_loaded) = store
        .current()
        .map(|snapshot| {
            let unified = snapshot
                .manifest()
                .component(ComponentKind::UnifiedGeo)
                .map(|c| c.size)
                .unwrap_or(0);
            (unified, snapshot.has_asn())
        })
        .unwrap_or((0, false));
    crate::metrics::analyzer::install_geo_store(store.clone(), unified_bytes, 0, asn_loaded);
}

fn components_of(manifest: &BundleManifest) -> Vec<AssetComponent> {
    manifest
        .components
        .iter()
        .map(|c| AssetComponent {
            kind: c.kind,
            version: c.version.clone(),
            size: c.size,
            sha256: c.sha256.clone(),
        })
        .collect()
}

/// The wire vocabulary of a component kind (matches the proto enum names).
pub fn component_kind_str(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::UnifiedGeo => "unified_geo",
        ComponentKind::CidrSets => "cidr_sets",
    }
}

fn asset_kind_wire(kind: &str) -> Result<i32> {
    Ok(match kind {
        "unified_geo" => AssetKind::UnifiedGeo as i32,
        "cidr_sets" => AssetKind::CidrSets as i32,
        other => bail!("unknown asset kind {other}"),
    })
}

/// Upper bound for one asset. The control plane refuses to publish anything
/// larger, and a truncated or runaway stream is refused rather than buffered.
const MAX_ASSET_BYTES: usize = 128 << 20;

type AssetClient = AssetControlClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        zelvik_suite::channel::CredentialInterceptor,
    >,
>;

/// The real AssetControl transport over the node's shared RPC channel.
pub struct GrpcAssetTransport {
    api_config: ApiConfig,
}

impl GrpcAssetTransport {
    pub fn new(api_config: ApiConfig) -> Self {
        Self { api_config }
    }

    async fn client(&self) -> Result<AssetClient> {
        let shared = SharedRpcClient::get(&self.api_config)
            .await
            .context("connect for geo asset control")?;
        // The asset plane is a role RPC too: a pending instance is refused
        // until it registers on the control channel.
        shared
            .ensure_registered()
            .await
            .context("register the edge instance for geo asset control")?;
        Ok(shared.as_rpc_client().asset_control_service())
    }

    fn envelope() -> zelvik_suite::pb::control::v1::EnvelopeMeta {
        zelvik_suite::pb::control::v1::EnvelopeMeta {
            schema_version: zelvik_suite::SCHEMA_VERSION_ASSET.to_string(),
            base_version: 0,
            patch_seq: 0,
            min_reader_version: zelvik_suite::PROTOCOL_VERSION.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl AssetTransport for GrpcAssetTransport {
    async fn get_asset_manifest(&mut self, known_generation: u64) -> Result<AssetManifestResponse> {
        let mut client = self.client().await?;
        let response = client
            .get_asset_manifest(GetAssetManifestRequest {
                meta: Some(Self::envelope()),
                node_id: self.api_config.instance_id.trim().to_string(),
                known_generation,
            })
            .await
            .context("GetAssetManifest")?
            .into_inner();
        let Some(bundle) = response.bundle else {
            return Ok(AssetManifestResponse {
                bundle_id: String::new(),
                generation: 0,
                manifest_json: Vec::new(),
                update_required: response.update_required,
                rollback_generation: response.rollback_generation,
            });
        };
        Ok(AssetManifestResponse {
            bundle_id: bundle.bundle_id,
            generation: bundle.generation,
            manifest_json: bundle.manifest_json,
            update_required: response.update_required,
            rollback_generation: response.rollback_generation,
        })
    }

    async fn download_asset(
        &mut self,
        bundle_id: &str,
        generation: u64,
        kind: &str,
    ) -> Result<Vec<u8>> {
        let mut client = self.client().await?;
        let response = client
            .download_asset(DownloadAssetRequest {
                meta: Some(Self::envelope()),
                node_id: self.api_config.instance_id.trim().to_string(),
                bundle_id: bundle_id.to_string(),
                generation,
                kind: asset_kind_wire(kind)?,
                from_chunk: 0,
            })
            .await
            .with_context(|| format!("DownloadAsset {kind}"))?;
        let mut stream = response.into_inner();
        let mut out: Vec<u8> = Vec::new();
        let mut expected_index: u32 = 0;
        loop {
            let Some(chunk) = stream.message().await.with_context(|| {
                format!("asset {kind} stream failed after {} bytes", out.len())
            })? else {
                bail!("asset {kind} stream ended without a final chunk");
            };
            if chunk.chunk_index != expected_index {
                bail!(
                    "asset {kind} chunks are not contiguous: got {}, expected {expected_index}",
                    chunk.chunk_index
                );
            }
            if chunk.bundle_id != bundle_id || chunk.generation != generation || chunk.kind != asset_kind_wire(kind)? {
                bail!("asset {kind} chunk identity disagrees with the request");
            }
            expected_index += 1;
            out.extend_from_slice(&chunk.chunk);
            if out.len() > MAX_ASSET_BYTES {
                bail!("asset {kind} exceeds {MAX_ASSET_BYTES} bytes");
            }
            if chunk.last {
                break;
            }
        }
        Ok(out)
    }

    async fn acknowledge_asset(&mut self, ack: AssetAck) -> Result<()> {
        let assets = ack
            .components
            .iter()
            .map(|c| -> Result<AssetDescriptor> {
                Ok(AssetDescriptor {
                    kind: asset_kind_wire(component_kind_str(c.kind))?,
                    version: c.version.clone(),
                    bytes: c.size,
                    sha256: c.sha256.clone(),
                    database_type: String::new(),
                    build_epoch: 0,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut client = self.client().await?;
        let response = client
            .acknowledge_asset(AcknowledgeAssetRequest {
                meta: Some(Self::envelope()),
                node_id: self.api_config.instance_id.trim().to_string(),
                bundle_id: ack.bundle_id,
                generation: ack.generation,
                assets,
                state: match ack.state {
                    AssetAckState::Active => AssetState::Active as i32,
                    AssetAckState::Staged => AssetState::Staged as i32,
                    AssetAckState::Failed => AssetState::Failed as i32,
                },
                error: ack.error,
            })
            .await
            .with_context(|| format!("AcknowledgeAsset {}", ack.state.as_str()))?
            .into_inner();
        if !response.accepted {
            bail!("AcknowledgeAsset was rejected");
        }
        Ok(())
    }
}

/// Runs the geo loop forever: reconcile, then sleep. A reconcile error is
/// logged and retried; the node keeps serving its last activated bundle.
pub async fn run_loop<T: AssetTransport + 'static>(
    mut agent: GeoAgent<T>,
    interval: std::time::Duration,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(interval);
    loop {
        ticker.tick().await;
        match agent.run_once().await {
            Ok(report) => {
                if report.activated {
                    info!(
                        bundle = %report.bundle_id,
                        generation = report.generation,
                        "geo bundle activated"
                    );
                }
            }
            Err(err) => warn!(error = %err, "geo reconcile failed; keeping last bundle"),
        }
    }
}

/// Opens the node's geo agent for the binary. `geo_root` defaults to
/// `<home>/data/geo`; a configured root must be the same path serve uses.
pub fn open_agent(api_config: ApiConfig, geo_root: std::path::PathBuf) -> Result<GeoAgent<GrpcAssetTransport>> {
    GeoAgent::open(GrpcAssetTransport::new(api_config), geo_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_kind_wire_is_closed() {
        assert_eq!(component_kind_str(ComponentKind::UnifiedGeo), "unified_geo");
        assert_eq!(component_kind_str(ComponentKind::CidrSets), "cidr_sets");
        assert_eq!(
            asset_kind_wire("unified_geo").expect("unified"),
            AssetKind::UnifiedGeo as i32
        );
        assert_eq!(
            AssetKind::UnifiedGeo as i32,
            5,
            "the published proto value must stay stable"
        );
        assert_eq!(
            asset_kind_wire("cidr_sets").expect("cidr"),
            AssetKind::CidrSets as i32
        );
        assert!(
            asset_kind_wire("city").is_err(),
            "retired kinds are not aliases"
        );
        assert!(asset_kind_wire("asn").is_err());
        assert!(asset_kind_wire("country").is_err());
        assert!(asset_kind_wire("provider").is_err());
    }
}
