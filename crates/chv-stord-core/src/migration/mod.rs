pub mod auth;
pub mod flow_control;
pub mod receiver;
pub mod sender;
pub mod service;
pub mod task;
pub mod tls_config;
pub mod volume_digest;

/// Maximum size of a single storage-migration gRPC message.
///
/// A `BlockChunk` carries up to one migration block
/// (`DIRTY_TRACKING_BLOCK_SIZE` = 4 MiB) of payload plus protobuf field
/// overhead, so it exceeds tonic's default 4 MiB decode limit — without a
/// raised limit the receiver rejects every real chunk with `OutOfRange`.
/// Both serving paths (mTLS TCP listener and Unix socket) accept messages
/// up to this size; the value gives 2x block headroom while staying
/// bounded.
pub const MAX_MIGRATION_MESSAGE_SIZE_BYTES: usize =
    2 * chv_stord_backends::DIRTY_TRACKING_BLOCK_SIZE as usize;

/// Test-support helpers shared across the migration module's unit tests.
#[cfg(test)]
pub mod tests {
    use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};

    /// A CA certificate PEM used by migration mTLS unit/negative tests.
    pub fn test_ca_pem() -> Vec<u8> {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "chv-test-ca");
        let cert = params.self_signed(&key).unwrap();
        cert.pem().into_bytes()
    }
}
