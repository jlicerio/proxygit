//! Optional mTLS helpers: generate a tiny CA + leaf client cert (DER).
//!
//! Used by `proxygit-client gen-mtls` and tests. Runtime wire-up lives in
//! server/client TLS config (`PROXYGIT_MTLS_CA`, `PROXYGIT_CLIENT_CERT`,
//! `PROXYGIT_CLIENT_KEY`).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};

/// Material produced by [`generate_mtls_bundle`].
pub struct MtlsBundle {
    pub ca_cert_der: Vec<u8>,
    pub ca_key_der: Vec<u8>,
    pub client_cert_der: Vec<u8>,
    pub client_key_der: Vec<u8>,
}

/// Generate a self-signed CA and one client leaf cert signed by it (DER/PKCS#8).
pub fn generate_mtls_bundle(client_cn: &str) -> Result<MtlsBundle> {
    let ca_key = KeyPair::generate().context("generate CA key")?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(DnType::CommonName, "ProxyGit mTLS CA");
    ca_params.distinguished_name = ca_dn;
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("self-sign mTLS CA")?;

    let client_key = KeyPair::generate().context("generate client key")?;
    let mut client_params = CertificateParams::default();
    client_params.is_ca = IsCa::NoCa;
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let mut client_dn = DistinguishedName::new();
    client_dn.push(DnType::CommonName, client_cn);
    client_params.distinguished_name = client_dn;
    let client_cert = client_params
        .signed_by(&client_key, &ca_cert, &ca_key)
        .context("sign client cert with CA")?;

    Ok(MtlsBundle {
        ca_cert_der: ca_cert.der().to_vec(),
        ca_key_der: ca_key.serialize_der(),
        client_cert_der: client_cert.der().to_vec(),
        client_key_der: client_key.serialize_der(),
    })
}

/// Write CA + client material into `out_dir` with fixed filenames.
///
/// Files:
/// - `ca_cert.der`, `ca_key.der` (mode 0600 on key)
/// - `client_cert.der`, `client_key.der` (mode 0600 on key)
pub fn write_mtls_bundle(out_dir: &Path, client_cn: &str) -> Result<MtlsBundle> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create mTLS out dir {}", out_dir.display()))?;
    let bundle = generate_mtls_bundle(client_cn)?;

    fs::write(out_dir.join("ca_cert.der"), &bundle.ca_cert_der)?;
    fs::write(out_dir.join("ca_key.der"), &bundle.ca_key_der)?;
    fs::write(out_dir.join("client_cert.der"), &bundle.client_cert_der)?;
    fs::write(out_dir.join("client_key.der"), &bundle.client_key_der)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            out_dir.join("ca_key.der"),
            fs::Permissions::from_mode(0o600),
        )?;
        fs::set_permissions(
            out_dir.join("client_key.der"),
            fs::Permissions::from_mode(0o600),
        )?;
    }

    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_roundtrip_files() {
        let dir = tempfile::tempdir().unwrap();
        let b = write_mtls_bundle(dir.path(), "test-client").unwrap();
        assert!(!b.ca_cert_der.is_empty());
        assert!(!b.client_cert_der.is_empty());
        assert!(dir.path().join("ca_cert.der").exists());
        assert!(dir.path().join("client_key.der").exists());
    }
}
