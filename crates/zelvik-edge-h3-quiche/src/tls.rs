//! Prevalidated TLS material; parsing happens during configuration publication.
use boring::{
    error::ErrorStack,
    ex_data::Index,
    pkey::{PKey, Private},
    ssl::{SelectCertError, Ssl, SslRef},
    x509::X509,
};

/// Per-context admission marker, checked before selecting/signing certificates.
/// Rejected initials still run the upstream closing stage to unmap their CIDs.
#[derive(Clone, Copy)]
pub struct HandshakeGate(Index<Ssl, bool>);
impl HandshakeGate {
    pub fn new() -> Result<Self, ErrorStack> {
        Ok(Self(Ssl::new_ex_index()?))
    }
    pub fn reject(&self, ssl: &mut SslRef) {
        ssl.set_ex_data(self.0, true);
    }
    pub fn check(&self, ssl: &SslRef) -> Result<(), SelectCertError> {
        if ssl.ex_data(self.0).copied().unwrap_or(false) {
            Err(SelectCertError::ERROR)
        } else {
            Ok(())
        }
    }
}

pub struct Certificate {
    chain: Vec<X509>,
    key: PKey<Private>,
}
impl std::fmt::Debug for Certificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicheCertificate")
            .field("chain_length", &self.chain.len())
            .finish_non_exhaustive()
    }
}
impl Certificate {
    pub fn from_der<'a>(
        chain: impl IntoIterator<Item = &'a [u8]>,
        key: &[u8],
    ) -> Result<Self, ErrorStack> {
        let chain: Vec<X509> = chain
            .into_iter()
            .map(X509::from_der)
            .collect::<Result<_, _>>()?;
        let key = PKey::private_key_from_der(key)?;
        // Validate with a temporary builder, not at the first client handshake.
        let mut context = boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls())?;
        if let Some(leaf) = chain.first() {
            context.set_certificate(leaf)?;
        }
        context.set_private_key(&key)?;
        context.check_private_key()?;
        Ok(Self { chain, key })
    }
    /// Apply once to a new SSL object. The caller pins the selected certificate
    /// in SSL ex-data so repeated ClientHello callbacks cannot append twice or
    /// mix two published configuration snapshots within the same handshake.
    pub fn apply_once(&self, ssl: &mut SslRef) -> Result<(), ErrorStack> {
        ssl.set_certificate(&self.chain[0])?;
        ssl.set_private_key(&self.key)?;
        for cert in &self.chain[1..] {
            ssl.add_chain_cert(cert)?;
        }
        Ok(())
    }
}
