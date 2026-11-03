//! Fail-closed admission with the upstream CID cleanup stage preserved.
use tokio::net::UdpSocket;
use tokio_quiche::{
    ApplicationOverQuic, InitialQuicConnection, QuicResult,
    metrics::Metrics,
    quic::{HandshakeInfo, QuicheConnection},
    quiche,
};

use crate::tls::HandshakeGate;

struct Rejected;
impl Rejected {
    fn stop(conn: &mut QuicheConnection) -> QuicResult<()> {
        match conn.close(false, 0x2, b"connection admission rejected") {
            Ok(()) | Err(quiche::Error::Done) => {}
            Err(error) => return Err(error.into()),
        }
        Err("connection admission rejected".into())
    }
}
impl ApplicationOverQuic for Rejected {
    fn on_conn_established(
        &mut self,
        conn: &mut QuicheConnection,
        _: &HandshakeInfo,
    ) -> QuicResult<()> {
        Self::stop(conn)
    }
    fn should_act(&self) -> bool {
        true
    }
    async fn wait_for_data(&mut self, conn: &mut QuicheConnection) -> QuicResult<()> {
        Self::stop(conn)
    }
    fn process_reads(&mut self, conn: &mut QuicheConnection) -> QuicResult<()> {
        Self::stop(conn)
    }
    fn process_writes(&mut self, conn: &mut QuicheConnection) -> QuicResult<()> {
        Self::stop(conn)
    }
}

/// Await cleanup before accepting another rejected initial, bounding rejection
/// work to one connection plus the configured backlog. This sends a TLS/QUIC
/// rejection, never application data. It deliberately does not promise silence.
/// Dropping an unstarted InitialQuicConnection or calling its ShutdownConnection
/// implementation does not unmap its CIDs in tokio-quiche 0.20.
pub async fn reject<M: Metrics>(
    mut incoming: InitialQuicConnection<UdpSocket, M>,
    gate: HandshakeGate,
) -> QuicResult<String> {
    gate.reject(incoming.ssl_mut());
    let (_, handshake) = incoming.handshake_fut(Rejected);
    match handshake.await {
        Err(reason) => Ok(reason.to_string()),
        Ok(worker) => {
            // Preserve cleanup even if upstream unexpectedly admits the TLS
            // handshake: the rejection application still refuses every stream.
            InitialQuicConnection::resume(worker);
            Err("rejected initial unexpectedly completed its handshake".into())
        }
    }
}
