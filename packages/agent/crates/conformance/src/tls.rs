//! A test CA made for each run, and one server certificate for every stubbed host.

use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// The CA the agent is told to trust, and the server side of the stubs.
pub struct TestPki {
    /// The CA certificate in PEM, mounted into the agent container.
    pub ca_pem: String,
    /// The TLS server config of the stubs, valid for `hosts`.
    pub server: Arc<ServerConfig>,
}

impl TestPki {
    /// A fresh CA and a leaf certificate for `hosts`.
    pub fn generate(hosts: &[&str]) -> Result<Self> {
        let ca_key = KeyPair::generate().context("CA key")?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).context("CA params")?;
        ca_params.distinguished_name.push(DnType::CommonName, "Ankor conformance test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        let ca_cert = ca_params.self_signed(&ca_key).context("CA certificate")?;
        let issuer = Issuer::new(ca_params, ca_key);

        let leaf_key = KeyPair::generate().context("leaf key")?;
        let mut leaf_params = CertificateParams::new(hosts.iter().map(|host| (*host).to_owned()).collect::<Vec<_>>()).context("leaf params")?;
        leaf_params.distinguished_name.push(DnType::CommonName, "Ankor conformance stubs");
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).context("leaf certificate")?;

        let chain = vec![CertificateDer::from(leaf_cert.der().to_vec()), CertificateDer::from(ca_cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("TLS versions")?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .context("server certificate")?;
        server.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(Self { ca_pem: ca_cert.pem(), server: Arc::new(server) })
    }
}
