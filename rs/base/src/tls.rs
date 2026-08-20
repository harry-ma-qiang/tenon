use anyhow::{bail, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::path::PathBuf;
use std::sync::Arc;

/// The TLS material a `serve --https` starts with. Either a cert/key pair the
/// human supplied as PEM, or an in-memory self-signed cert rcgen mints for dev,
/// whose SHA-256 fingerprint is printed so a `curl --cacert`/`-k` client can
/// pin it.
pub fn server_config(cert: Option<PathBuf>, key: Option<PathBuf>) -> Result<Arc<ServerConfig>> {
    let (certs, key) = match (cert, key) {
        (Some(cert), Some(key)) => load_pem(&cert, &key)?,
        (None, None) => self_signed()?,
        _ => bail!("--https needs both --cert and --key, or neither for a dev self-signed cert"),
    };
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("tls protocol versions")?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("tls certificate")?;
    Ok(Arc::new(config))
}

fn self_signed() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let generated = rcgen::generate_simple_self_signed(names).context("generate self-signed")?;
    let der = generated.cert.der().clone();
    println!("tenon: self-signed cert sha-256 {}", fingerprint(&der));
    let key = PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der());
    Ok((vec![der], PrivateKeyDer::Pkcs8(key)))
}

fn load_pem(
    cert: &PathBuf,
    key: &PathBuf,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = std::fs::read(cert).with_context(|| format!("read {}", cert.display()))?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<_, _>>()
        .with_context(|| format!("parse {}", cert.display()))?;
    if certs.is_empty() {
        bail!("no certificate in {}", cert.display());
    }
    let key_pem = std::fs::read(key).with_context(|| format!("read {}", key.display()))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .with_context(|| format!("parse {}", key.display()))?
        .with_context(|| format!("no private key in {}", key.display()))?;
    Ok((certs, key))
}

fn fingerprint(der: &CertificateDer<'_>) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(der.as_ref());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
