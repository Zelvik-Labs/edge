//! Opt-in native tokio-quiche listener feeding the existing CDN/cache handler.
use super::*;
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use zelvik_edge_h3_quiche::{
    Limits,
    boring::ssl::SslContextBuilder,
    tokio_quiche::{
        self, ConnectionParams,
        metrics::DefaultMetrics,
        quic::ConnectionHook,
        settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths},
    },
};

struct PreparedTls(Mutex<Option<SslContextBuilder>>);
impl ConnectionHook for PreparedTls {
    fn create_custom_ssl_context_builder(
        &self,
        _: TlsCertificatePaths<'_>,
    ) -> Option<SslContextBuilder> {
        Some(
            self.0
                .lock()
                .take()
                .expect("one prevalidated TLS builder per native UDP socket"),
        )
    }
}
// Upstream queues 2048 incoming Vecs, each potentially 64KiB with GRO. The
// bounded listener enforces <=1500-byte datagrams before this queue, with GRO
// off and GSO preserved. Reserve payload budgets plus metadata headroom;
// ordinary connection admission remains separately charged as well.
const TRANSPORT_RESERVATION: u64 = 5 * 1024 * 1024;
const LISTEN_BACKLOG: usize = 32;
struct TransportReservation(u64);
impl Drop for TransportReservation {
    fn drop(&mut self) {
        MEMORY_GOVERNOR.workspace_refund(self.0);
    }
}

impl Http3ProxyManager {
    fn validate_quiche_policy(&self) -> Result<()> {
        let policy = self.config_store.get_global_http3_policy_sync();
        anyhow::ensure!(
            policy.as_ref().is_some_and(|p| p.address_validation_mode()
                == crate::config_models::Http3AddressValidation::Off),
            "tokio-quiche native experiment supports only an explicitly configured addressValidation=off; its native Retry API cannot preserve Edge's adaptive node-wide Retry budget"
        );
        Ok(())
    }

    pub(super) async fn run_quiche_socket(
        self: Arc<Self>,
        socket: UdpSocket,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        self.validate_quiche_policy()?;
        let address = socket.local_addr()?;
        let (builder, handshake_gate) = self.cert_selector.build_quiche_ssl_builder()?;
        // The library queues initials before our per-connection admission.
        // Reserve the full bounded backlog plus one inline rejection worker.
        let backlog_bytes = (LISTEN_BACKLOG as u64 + 1) * TRANSPORT_RESERVATION;
        anyhow::ensure!(
            MEMORY_GOVERNOR.try_admit_workspace(backlog_bytes),
            "insufficient workspace budget for the native QUIC accept backlog"
        );
        let _backlog = TransportReservation(backlog_bytes);
        let mut settings = QuicSettings::default();
        settings.enable_dgram = false;
        settings.enable_early_data = false;
        settings.disable_client_ip_validation = true; // validated explicit Off policy above
        settings.initial_max_streams_bidi =
            MEMORY_GOVERNOR.h3_request_limit_per_connection().max(1) as u64;
        settings.initial_max_streams_uni = 8;
        settings.initial_max_data = 256 * 1024;
        settings.max_connection_window = 256 * 1024;
        settings.initial_max_stream_data_bidi_local = 64 * 1024;
        settings.initial_max_stream_data_bidi_remote = 64 * 1024;
        settings.initial_max_stream_data_uni = 64 * 1024;
        settings.max_stream_window = 64 * 1024;
        settings.cc_algorithm = "cubic".into();
        settings.initial_congestion_window_packets = 10;
        settings.max_send_udp_payload_size = 1350;
        settings.max_recv_udp_payload_size = 1350;
        settings.discover_path_mtu = false;
        settings.disable_active_migration = true;
        settings.handshake_timeout = Some(std::time::Duration::from_secs(10));
        settings.max_idle_timeout = Some(std::time::Duration::from_secs(30));
        settings.listen_backlog = LISTEN_BACKLOG;
        let hooks = Hooks {
            connection_hook: Some(Arc::new(PreparedTls(Mutex::new(Some(builder))))),
        };
        // The upstream hook requires Some(paths), but our prepared builder is
        // supplied before any file lookup. No private material is written here.
        let params = ConnectionParams::new_server(
            settings,
            TlsCertificatePaths {
                cert: "dynamic-selector",
                private_key: "dynamic-selector",
                kind: CertificateKind::X509,
            },
            hooks,
        );
        let (bounded, discarded) = zelvik_edge_h3_quiche::ingress::bounded_listener(socket)
            .context("configure bounded native QUIC ingress")?;
        info!(
            "HTTP/3 native ingress: UDP <=1500 bytes, GRO off, GSO on, pre-readiness discarded={discarded}"
        );
        let mut listeners =
            tokio_quiche::listen_with_capabilities([bounded], params, DefaultMetrics)?;
        let mut listener = listeners.pop().context("native QUIC listener missing")?;
        let mut logic = self.proxy_logic.clone();
        logic.tls_downstream = true;
        let proxy = Arc::new(http_proxy_custom(
            &self.server_conf,
            logic,
            crate::origin_h3::OriginH3Connector,
        ));
        let mut admission_diagnostic_reported = false;
        let mut report_admission = |class: &'static str| {
            if !admission_diagnostic_reported {
                admission_diagnostic_reported = true;
                tracing::warn!(
                    %address,
                    class,
                    workspace_used = MEMORY_GOVERNOR.request_workspace_used_bytes(),
                    account = ?MEMORY_GOVERNOR.account_view(),
                    "HTTP/3 native first admission rejection; subsequent rejections retain L4 events"
                );
            }
        };
        info!("HTTP/3 experimental tokio-quiche 0.20.0 + Hyperium h3 listener on {address}");
        loop {
            let incoming = tokio::select! {
                _ = shutdown.changed() => {
                    listener.close();
                    while let Some(queued) = listener.next().await {
                        match queued {
                            Ok(initial) => {
                                let reason = zelvik_edge_h3_quiche::admission::reject(initial, handshake_gate)
                                    .await.map_err(|error| anyhow::anyhow!(error))?;
                                debug!("HTTP/3 native queued initial rejected during shutdown: {reason}");
                            }
                            Err(error) => error!("HTTP/3 native queued initial failed during shutdown: {error}"),
                        }
                    }
                    return Ok(());
                },
                incoming = listener.next() => incoming,
            };
            let Some(incoming) = incoming else {
                anyhow::bail!("native QUIC listener ended unexpectedly");
            };
            let incoming = match incoming {
                Ok(incoming) => incoming,
                Err(error) => {
                    error!("HTTP/3 native QUIC initial rejected on {address}: {error}");
                    continue;
                }
            };
            if let Err(error) = self.validate_quiche_policy() {
                let reason = zelvik_edge_h3_quiche::admission::reject(incoming, handshake_gate)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
                error!("HTTP/3 native policy rejects new initial: {error}; {reason}");
                continue;
            }
            let remote = incoming.peer_addr();
            if crate::l4_defense::is_l4_blocked(
                &self.config_store,
                &self.proxy_logic.waf_state,
                remote.ip(),
            ) {
                H3_IGNORED_INCOMING.fetch_add(1, Ordering::Relaxed);
                let reason = zelvik_edge_h3_quiche::admission::reject(incoming, handshake_gate)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
                debug!("HTTP/3 native blocked peer {remote} rejected: {reason}");
                continue;
            }
            let Some(connection_permit) =
                MEMORY_GOVERNOR.try_admit(AdmissionClass::Http3Connection)
            else {
                report_admission("quiche_connection");
                self.record_l4_event(
                    remote.ip(),
                    L4DefenseKind::H3AdmissionReject,
                    format!(
                        "port={} peer={remote} class=quiche_connection",
                        address.port()
                    ),
                );
                let reason = zelvik_edge_h3_quiche::admission::reject(incoming, handshake_gate)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
                debug!("HTTP/3 native admission rejection for {remote}: {reason}");
                continue;
            };
            let listener_key = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), address.port());
            let Some(listener_permit) =
                MEMORY_GOVERNOR.try_admit_listener(listener_key, AdmissionClass::Http3Connection)
            else {
                report_admission("quiche_listener");
                self.record_l4_event(
                    remote.ip(),
                    L4DefenseKind::H3AdmissionReject,
                    format!(
                        "port={} peer={remote} class=quiche_listener",
                        address.port()
                    ),
                );
                let reason = zelvik_edge_h3_quiche::admission::reject(incoming, handshake_gate)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
                debug!("HTTP/3 native admission rejection for {remote}: {reason}");
                continue;
            };
            if !MEMORY_GOVERNOR.try_admit_workspace(TRANSPORT_RESERVATION) {
                report_admission("quiche_transport_bytes");
                MEMORY_GOVERNOR.record_reject(AdmissionClass::Http3Connection);
                self.record_l4_event(
                    remote.ip(),
                    L4DefenseKind::H3AdmissionReject,
                    format!(
                        "port={} peer={remote} class=quiche_transport_bytes",
                        address.port()
                    ),
                );
                let reason = zelvik_edge_h3_quiche::admission::reject(incoming, handshake_gate)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
                debug!("HTTP/3 native admission rejection for {remote}: {reason}");
                continue;
            }
            let owner = Arc::new((
                connection_permit,
                listener_permit,
                TransportReservation(TRANSPORT_RESERVATION),
                crate::metrics::transport_metrics_guard(
                    crate::metrics::ShadowTransportKind::QuicConnection,
                ),
            ));
            let limits = Limits {
                streams: MEMORY_GOVERNOR.h3_request_limit_per_connection().max(1) + 16,
                ..Limits::default()
            };
            let (driver, transport, close) =
                zelvik_edge_h3_quiche::server_with_owner(limits, owner)
                    .map_err(|error| anyhow::anyhow!(error))?;
            let manager = self.clone();
            let proxy = proxy.clone();
            let mut request_shutdown = shutdown.clone();
            tokio::spawn(async move {
                let _drain = H3ConnectionRegistration::register_transport(
                    H3LiveConnection::Quiche(close.clone()),
                );
                let handler_shutdown = request_shutdown.clone();
                let serve = async move {
                    // start owns handshake -> running -> closing as one lifecycle.
                    // Cancelling our H3 handler cannot drop a paused Running worker.
                    let _metadata = incoming.start(driver);
                    manager
                        .serve_h3_transport(
                            transport,
                            address.port(),
                            remote,
                            proxy,
                            handler_shutdown,
                        )
                        .await
                };
                tokio::select! {
                    result = serve => if let Err(error) = result { debug!("HTTP/3 native quiche connection on {address} failed: {error}"); },
                    _ = request_shutdown.changed() => {},
                }
                close.close(
                    h3::error::Code::H3_NO_ERROR.value(),
                    b"native H3 handler ended",
                );
            });
        }
    }
}
