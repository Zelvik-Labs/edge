//! Edge's geo asset agent against the real merged `Zelvik-Geo` database.
//!
//! The fixture is the raw artifact the license pipeline produces and the
//! control plane verifies: one MMDB carrying the City, Country and ASN source
//! records. These tests cover the node-facing half of that pipeline: the node
//! only ever talks to the authenticated `AssetControl` surface, installs the
//! unified database from one authenticated generation, and leaves the previous
//! generation serving when a new generation is partial, corrupt, or reuses an
//! installed identity with different bytes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use zelvik_edge::geo::{
    AssetAck, AssetAckState, AssetManifestResponse, AssetTransport, GeoAgent,
    component_kind_str,
};
use zelvik_geo_runtime::{BundleManifest, ComponentKind, ComponentManifest, GeoStore};

const UNIFIED_GEO_FIXTURE: &str = "tests/testdata/mmdb/zelvik-geo.mmdb";

fn digest_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("read fixture {path}: {err}"))
}

fn unified_geo_bytes() -> Vec<u8> {
    fixture(UNIFIED_GEO_FIXTURE)
}

/// Opt-in production-scale artifact. `ZELVIK_TEST_UNIFIED_GEO_MMDB` points at
/// the exact bytes the control plane published, so the same node-facing path
/// (download, staging, container parse, lookup, ACK) runs against a
/// real generation instead of only the small committed fixture.
fn published_artifact() -> Option<Vec<u8>> {
    let path = std::env::var_os("ZELVIK_TEST_UNIFIED_GEO_MMDB")?;
    let path = path.to_string_lossy().into_owned();
    let bytes = fixture(&path);
    assert!(
        bytes.starts_with(b"ZVGEO\0\0\x01"),
        "{path} is not a ZVGEO format-1 container"
    );
    Some(bytes)
}

fn component(kind: ComponentKind, version: &str, bytes: &[u8]) -> ComponentManifest {
    ComponentManifest {
        kind,
        version: version.to_string(),
        size: bytes.len() as u64,
        sha256: digest_of(bytes),
        database_type: None,
        build_epoch: None,
    }
}

fn manifest_with(
    bundle_id: &str,
    generation: u64,
    components: Vec<ComponentManifest>,
) -> BundleManifest {
    let mut manifest = BundleManifest {
        bundle_id: bundle_id.to_string(),
        generation,
        components,
        manifest_sha256: String::new(),
    };
    let canonical = manifest.canonical_json().expect("canonical json");
    manifest.manifest_sha256 = digest_of(&canonical);
    manifest
}

fn manifest_for(bundle_id: &str, generation: u64, unified_geo: &[u8]) -> BundleManifest {
    manifest_with(
        bundle_id,
        generation,
        vec![component(ComponentKind::UnifiedGeo, "geo-1", unified_geo)],
    )
}

fn components_with_cidr(unified_geo: &[u8], cidr_sets: &[u8]) -> Vec<ComponentManifest> {
    vec![
        component(ComponentKind::UnifiedGeo, "2026.10.24", unified_geo),
        component(ComponentKind::CidrSets, "2026.10.24", cidr_sets),
    ]
}

#[derive(Default)]
struct Recording {
    acks: Vec<AssetAck>,
    download_kinds: Vec<String>,
    fail_failed_ack: bool,
}

struct FakeTransport {
    response: AssetManifestResponse,
    assets: BTreeMap<String, Vec<u8>>,
    record: Arc<Mutex<Recording>>,
}

impl FakeTransport {
    fn set_asset(&mut self, kind: &str, bytes: Vec<u8>) {
        self.assets.insert(kind.to_string(), bytes);
    }

    fn remove_asset(&mut self, kind: &str) {
        self.assets.remove(kind);
    }

    fn tamper_asset(&mut self, kind: &str) {
        let bytes = self
            .assets
            .get_mut(kind)
            .unwrap_or_else(|| panic!("no {kind} asset to tamper with"));
        bytes[0] ^= 0xff;
    }
}

#[async_trait::async_trait]
impl AssetTransport for FakeTransport {
    async fn get_asset_manifest(
        &mut self,
        _known_generation: u64,
    ) -> anyhow::Result<AssetManifestResponse> {
        Ok(self.response.clone())
    }

    async fn download_asset(
        &mut self,
        _bundle_id: &str,
        _generation: u64,
        kind: &str,
    ) -> anyhow::Result<Vec<u8>> {
        {
            let mut record = self.record.lock().unwrap();
            record.download_kinds.push(kind.to_string());
        }
        self.assets
            .get(kind)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no {kind} asset for this generation"))
    }

    async fn acknowledge_asset(&mut self, ack: AssetAck) -> anyhow::Result<()> {
        let mut record = self.record.lock().unwrap();
        let fail = record.fail_failed_ack && ack.state == AssetAckState::Failed;
        record.acks.push(ack);
        if fail {
            anyhow::bail!("asset ACK transport unavailable");
        }
        Ok(())
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = PathBuf::from("/tmp").join(format!(
            "zeg-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("tempdir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    agent: GeoAgent<FakeTransport>,
    record: Arc<Mutex<Recording>>,
    root: TempDir,
}

fn response(manifest: &BundleManifest) -> AssetManifestResponse {
    AssetManifestResponse {
        bundle_id: manifest.bundle_id.clone(),
        generation: manifest.generation,
        manifest_json: serde_json::to_vec(manifest).expect("manifest json"),
        update_required: true,
        rollback_generation: 0,
    }
}

fn harness(tag: &str) -> Harness {
    let initial = manifest_for("gb_one", 5, b"placeholder");
    let record = Arc::new(Mutex::new(Recording::default()));
    let transport = FakeTransport {
        response: response(&initial),
        assets: BTreeMap::from([("unified_geo".to_string(), b"placeholder".to_vec())]),
        record: record.clone(),
    };
    let root = TempDir::new(tag);
    let agent = GeoAgent::open(transport, root.path()).expect("open geo agent");
    Harness { agent, record, root }
}

fn set_generation_assets(h: &mut Harness, unified_geo: &[u8], cidr_sets: &[u8]) {
    let transport = h.agent.transport_mut();
    transport.set_asset("unified_geo", unified_geo.to_vec());
    transport.set_asset("cidr_sets", cidr_sets.to_vec());
}

async fn activate_unified(h: &mut Harness, unified_geo: &[u8]) {
    h.agent
        .transport_mut()
        .set_asset("unified_geo", unified_geo.to_vec());
    h.agent.transport_mut().response = response(&manifest_for("gb_one", 5, unified_geo));
    let report = h.agent.run_once().await.expect("activate unified bundle");
    assert!(report.activated);
    assert_eq!(h.agent.active_generation(), 5);
}

fn active_store(root: &Path) -> (u64, String) {
    let store = GeoStore::open_readonly(root).expect("open installed bundle");
    (store.generation(), store.active_bundle_id())
}

#[tokio::test]
async fn no_publication_needs_no_trust_key_or_acknowledgement() {
    let mut h = harness("no-publication");
    h.agent.transport_mut().response.bundle_id.clear();
    h.agent.transport_mut().response.generation = 0;
    let report = h.agent.run_once().await.expect("no publication");
    assert!(report.unchanged);
    assert_eq!(report.generation, 0);
    let rec = h.record.lock().unwrap();
    assert!(rec.acks.is_empty());
    assert!(rec.download_kinds.is_empty());
}

#[tokio::test]
async fn a_unified_artifact_answers_city_country_and_asn_from_one_generation() {
    let unified_geo = unified_geo_bytes();
    let mut h = harness("unified");
    activate_unified(&mut h, &unified_geo).await;

    assert_eq!(h.agent.active_bundle_id(), "gb_one");
    let (generation, bundle_id) = active_store(h.root.path());
    assert_eq!((generation, bundle_id.as_str()), (5, "gb_one"));
    let store = GeoStore::open_readonly(h.root.path()).expect("installed bundle");
    let view = store.current().expect("active snapshot");
    assert!(view.has_city());
    assert!(view.has_country());
    assert!(view.has_asn());

    let located = view.lookup("81.2.69.142".parse().expect("ip"));
    assert_eq!(located.generation, 5);
    assert_eq!(&*located.bundle_id, "gb_one");
    let record = located.record.expect("record");
    assert_eq!(record.country_iso.as_deref(), Some("GB"));
    assert!(record.city_id.is_some(), "City id must be present");

    let routed = view.lookup("1.0.0.1".parse().expect("ip"));
    assert_eq!(routed.generation, 5);
    assert_eq!(&*routed.bundle_id, "gb_one");
    assert_eq!(routed.record.expect("ASN record").asn, Some(15169));

    // One address carried by both sources must report country and ASN from
    // the same snapshot, never from two independently refreshed databases.
    let coherent = view.lookup("89.160.20.112".parse().expect("ip"));
    let record = coherent.record.expect("merged record");
    assert_eq!(record.country_iso.as_deref(), Some("SE"));
    assert_eq!(record.asn, Some(29518));

    let rec = h.record.lock().unwrap();
    assert_eq!(rec.download_kinds, vec!["unified_geo".to_string()]);
    assert_eq!(rec.acks.len(), 1);
    let ack = &rec.acks[0];
    assert_eq!(ack.state, AssetAckState::Active);
    assert_eq!(ack.bundle_id, "gb_one");
    assert_eq!(ack.generation, 5);
    let kinds = ack
        .components
        .iter()
        .map(|c| component_kind_str(c.kind))
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec!["unified_geo"]);
    assert!(ack.components.iter().all(|c| c.version == "geo-1"));
}

/// Opt-in: the full node-facing path against the real published container.
///
/// `81.2.69.142` is a real UK allocation carried by all three source databases
/// in the published artifact (GB / Milton Keynes / AS20712), so one lookup
/// proves the City, Country and ASN readers were loaded from the same
/// generation. Without `ZELVIK_TEST_UNIFIED_GEO_MMDB` the deterministic fixture
/// test above is the default and this test does not run.
#[tokio::test]
async fn a_published_container_answers_country_city_and_asn() {
    let Some(unified_geo) = published_artifact() else {
        return;
    };
    let mut h = harness("published");
    activate_unified(&mut h, &unified_geo).await;

    let store = GeoStore::open_readonly(h.root.path()).expect("installed published bundle");
    let view = store.current().expect("active snapshot");
    assert!(view.has_city(), "published container has no City source");
    assert!(
        view.has_country(),
        "published container has no Country source"
    );
    assert!(view.has_asn(), "published container has no ASN source");

    let located = view.lookup("81.2.69.142".parse().expect("ip"));
    assert_eq!(located.generation, 5);
    let record = located.record.expect("81.2.69.142 must be in the database");
    assert_eq!(record.country_iso.as_deref(), Some("GB"));
    assert_eq!(record.asn, Some(20712));
    assert!(
        record.city_name.is_some() || record.city_id.is_some(),
        "City source did not answer: {record:?}"
    );

    let rec = h.record.lock().unwrap();
    assert_eq!(rec.download_kinds, vec!["unified_geo".to_string()]);
    assert_eq!(rec.acks.len(), 1);
    let ack = &rec.acks[0];
    assert_eq!(ack.state, AssetAckState::Active);
    assert_eq!((ack.bundle_id.as_str(), ack.generation), ("gb_one", 5));
    assert_eq!(ack.components.len(), 1);
    assert_eq!(
        component_kind_str(ack.components[0].kind),
        "unified_geo",
        "the ACK must name the exact component that was installed"
    );
}

#[tokio::test]
async fn a_missing_cidr_set_keeps_the_previous_generation_active() {
    let unified_geo = unified_geo_bytes();
    let mut h = harness("partial");
    activate_unified(&mut h, &unified_geo).await;

    h.agent.transport_mut().response = response(&manifest_with(
        "gb_two",
        6,
        components_with_cidr(&unified_geo, b"cidr-placeholder"),
    ));
    set_generation_assets(&mut h, &unified_geo, b"cidr-placeholder");
    h.agent.transport_mut().remove_asset("cidr_sets");
    let err = h.agent.run_once().await.expect_err("missing component");
    assert!(
        format!("{err:#}").contains("download cidr_sets asset"),
        "unexpected error: {err:#}"
    );

    assert_eq!(h.agent.active_generation(), 5);
    assert_eq!(h.agent.active_bundle_id(), "gb_one");
    let (generation, bundle_id) = active_store(h.root.path());
    assert_eq!((generation, bundle_id.as_str()), (5, "gb_one"));

    let rec = h.record.lock().unwrap();
    assert_eq!(
        rec.download_kinds[1..]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["unified_geo", "cidr_sets"]
    );
    assert_eq!(rec.acks.len(), 2);
    let ack = &rec.acks[1];
    assert_eq!(ack.state, AssetAckState::Failed);
    assert_eq!(ack.bundle_id, "gb_two");
    assert_eq!(ack.generation, 6);
    assert_eq!(
        ack.components.len(),
        2,
        "a failure ACK must describe the whole generation"
    );
    assert!(ack.error.contains("cidr_sets"), "{:?}", ack.error);
    assert!(
        std::fs::read_dir(h.root.path().join("staging"))
            .expect("staging dir")
            .next()
            .is_none(),
        "a failed generation must leave no staged bytes"
    );
    assert!(
        !h.root.path().join("bundles/gb_two").exists(),
        "a failed generation must not be committed under its bundle id"
    );
}

#[tokio::test]
async fn a_bad_component_digest_keeps_the_previous_generation_active() {
    let unified_geo = unified_geo_bytes();
    let mut h = harness("bad-digest");
    activate_unified(&mut h, &unified_geo).await;

    // The unified database downloads first; the CIDR digest fails afterwards.
    // The whole generation must still be discarded.
    h.agent.transport_mut().response = response(&manifest_with(
        "gb_two",
        6,
        components_with_cidr(&unified_geo, b"cidr-placeholder"),
    ));
    set_generation_assets(&mut h, &unified_geo, b"cidr-placeholder");
    h.agent.transport_mut().tamper_asset("cidr_sets");
    let err = h.agent.run_once().await.expect_err("digest mismatch");
    assert!(
        format!("{err:#}").contains("cidr_sets asset digest mismatch"),
        "unexpected error: {err:#}"
    );

    assert_eq!(h.agent.active_generation(), 5);
    assert_eq!(h.agent.active_bundle_id(), "gb_one");
    let store = GeoStore::open_readonly(h.root.path()).expect("installed bundle");
    let view = store.current().expect("active snapshot");
    assert_eq!(
        view.lookup("1.0.0.1".parse().expect("ip"))
            .record
            .expect("ASN record")
            .asn,
        Some(15169)
    );

    let rec = h.record.lock().unwrap();
    assert_eq!(
        rec.download_kinds[1..]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["unified_geo", "cidr_sets"]
    );
    assert_eq!(rec.acks.len(), 2);
    assert_eq!(rec.acks[1].state, AssetAckState::Failed);
    assert_eq!(rec.acks[1].generation, 6);
    assert!(rec.acks[1].error.contains("digest mismatch"));
}

/// Retired component kinds are not aliases. A manifest that still names them
/// must fail deserialization before any download work.
#[tokio::test]
async fn a_retired_component_kind_is_refused_not_aliased() {
    let mut h = harness("retired-kind");
    let manifest = manifest_for("gb_one", 5, b"placeholder");
    let mut value = serde_json::to_value(&manifest).expect("manifest value");
    value["components"][0]["kind"] = serde_json::json!("city");
    h.agent.transport_mut().response.manifest_json =
        serde_json::to_vec(&value).expect("manifest json");
    let err = h.agent.run_once().await.expect_err("retired kind");
    let text = format!("{err:#}");
    assert!(text.contains("unknown variant `city`"), "{text}");
    assert!(h.record.lock().unwrap().download_kinds.is_empty());
}

/// A reused `(bundle_id, generation)` with different contents must be
/// refused. Replacing the active generation would make the control plane's
/// "unchanged generation" path silently serve different bytes.
#[tokio::test]
async fn reusing_an_installed_identity_with_a_different_manifest_is_refused() {
    let unified_geo = unified_geo_bytes();
    let mut h = harness("duplicate-identity");
    activate_unified(&mut h, &unified_geo).await;
    let original = manifest_for("gb_one", 5, &unified_geo);

    let before = {
        let rec = h.record.lock().unwrap();
        (rec.download_kinds.len(), rec.acks.len())
    };
    // Same bundle id, generation and component version, but different
    // component bytes: reusing the identity would silently change what the
    // "unchanged" generation serves.
    let mut changed = unified_geo.clone();
    changed[0] ^= 0xff;
    let reused = manifest_with(
        "gb_one",
        5,
        vec![component(ComponentKind::UnifiedGeo, "geo-1", &changed)],
    );
    assert_ne!(reused.manifest_sha256, original.manifest_sha256);
    h.agent.transport_mut().response = response(&reused);
    let err = h.agent.run_once().await.expect_err("identity reuse");
    assert!(
        err.to_string()
            .contains("identity was reused with a different manifest"),
        "unexpected error: {err:#}"
    );

    let rec = h.record.lock().unwrap();
    assert_eq!(
        (rec.download_kinds.len(), rec.acks.len()),
        before,
        "a reused identity must not download or acknowledge again"
    );
    assert_eq!(h.agent.active_generation(), 5);
    assert_eq!(h.agent.active_bundle_id(), "gb_one");
}
