use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

use crate::utils::fix_ownership;

use super::sni::{sanitize_hostname_for_filename, validate_sni_hostname};
use super::{
    CA_CERT_FILE, CA_COMMON_NAME, CA_KEY_FILE, CA_VALIDITY_DAYS, HOST_CERTS_DIR, SERVER_CERT_FILE,
    SERVER_KEY_FILE, SERVER_VALIDITY_DAYS, assert_path_containment, is_local_domain,
    load_ca_keypair, set_file_mode, write_key_file,
};

// ---------------------------------------------------------------------------
// Certificate generation
// ---------------------------------------------------------------------------

/// Generate a self-signed CA certificate and key, writing PEM files to
/// `state_dir`. Returns `(ca_cert_path, ca_key_path)`.
pub fn generate_ca(state_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    fs::create_dir_all(state_dir)?;

    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;

    let mut params = CertificateParams::default();
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, CA_COMMON_NAME);
        dn.push(DnType::OrganizationName, "nsl");
        dn
    };
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = time::OffsetDateTime::now_utc()
        .checked_sub(time::Duration::days(1))
        .expect("date arithmetic overflow");
    params.not_after = time::OffsetDateTime::now_utc()
        .checked_add(time::Duration::days(CA_VALIDITY_DAYS as i64))
        .expect("CA validity duration overflow");

    let ca_cert = params.self_signed(&key_pair)?;

    let ca_key_path = state_dir.join(CA_KEY_FILE);
    let ca_cert_path = state_dir.join(CA_CERT_FILE);

    write_key_file(&ca_key_path, &key_pair.serialize_pem())?;
    fix_ownership(&ca_key_path);

    fs::write(&ca_cert_path, ca_cert.pem())?;
    set_file_mode(&ca_cert_path, 0o644);
    fix_ownership(&ca_cert_path);

    tracing::info!("generated CA certificate at {}", ca_cert_path.display());
    Ok((ca_cert_path, ca_key_path))
}

/// Generate a server certificate signed by the local CA. The cert covers
/// `localhost` and `*.localhost`. Returns `(cert_path, key_path)`.
pub fn generate_server_cert(state_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let ca_issuer = load_ca_keypair(state_dir)?;

    let server_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;

    let mut params = CertificateParams::default();
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "localhost");
        dn
    };
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into()?),
        SanType::DnsName("*.localhost".try_into()?),
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = time::OffsetDateTime::now_utc()
        .checked_sub(time::Duration::days(1))
        .expect("date arithmetic overflow");
    params.not_after = time::OffsetDateTime::now_utc()
        .checked_add(time::Duration::days(SERVER_VALIDITY_DAYS as i64))
        .expect("server cert validity duration overflow");

    let server_cert = params.signed_by(&server_key, &ca_issuer)?;

    let cert_path = state_dir.join(SERVER_CERT_FILE);
    let key_path = state_dir.join(SERVER_KEY_FILE);

    write_key_file(&key_path, &server_key.serialize_pem())?;
    fix_ownership(&key_path);

    fs::write(&cert_path, server_cert.pem())?;
    set_file_mode(&cert_path, 0o644);
    fix_ownership(&cert_path);

    tracing::info!("generated server certificate at {}", cert_path.display());
    Ok((cert_path, key_path))
}

/// Generate a per-hostname certificate signed by the local CA. Certificates
/// are cached in `state_dir/host-certs/`. Returns `(cert_path, key_path)`.
pub fn generate_host_cert(state_dir: &Path, hostname: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    anyhow::ensure!(
        validate_sni_hostname(hostname),
        "invalid hostname for certificate generation: {:?}",
        hostname
    );

    let host_dir = state_dir.join(HOST_CERTS_DIR);
    fs::create_dir_all(&host_dir)?;
    fix_ownership(&host_dir);

    let ca_issuer = load_ca_keypair(state_dir)?;

    let host_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;

    let mut sans = vec![SanType::DnsName(hostname.try_into()?)];
    if let Some(pos) = hostname.find('.') {
        let parent = &hostname[pos + 1..];
        if is_local_domain(parent) {
            let wildcard = format!("*.{}", parent);
            sans.push(SanType::DnsName(wildcard.as_str().try_into()?));
        }
    }

    let mut params = CertificateParams::default();
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, hostname);
        dn
    };
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = time::OffsetDateTime::now_utc()
        .checked_sub(time::Duration::days(1))
        .expect("date arithmetic overflow");
    params.not_after = time::OffsetDateTime::now_utc()
        .checked_add(time::Duration::days(SERVER_VALIDITY_DAYS as i64))
        .expect("server cert validity duration overflow");

    let host_cert = params.signed_by(&host_key, &ca_issuer)?;

    let safe_name = sanitize_hostname_for_filename(hostname);
    let cert_path = host_dir.join(format!("{}.pem", safe_name));
    let key_path = host_dir.join(format!("{}-key.pem", safe_name));

    assert_path_containment(&cert_path, &host_dir)?;
    assert_path_containment(&key_path, &host_dir)?;

    write_key_file(&key_path, &host_key.serialize_pem())?;
    fix_ownership(&key_path);

    fs::write(&cert_path, host_cert.pem())?;
    set_file_mode(&cert_path, 0o644);
    fix_ownership(&cert_path);

    tracing::debug!(
        "generated host certificate for {} at {}",
        hostname,
        cert_path.display()
    );
    Ok((cert_path, key_path))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_host_cert_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        generate_ca(tmp.path()).unwrap();
        assert!(generate_host_cert(tmp.path(), "../../etc/passwd").is_err());
        assert!(generate_host_cert(tmp.path(), "foo/bar").is_err());
    }

    #[test]
    fn test_generate_ca_creates_files() {
        let tmp = TempDir::new().unwrap();
        let (cert_path, key_path) = generate_ca(tmp.path()).unwrap();

        assert!(cert_path.exists(), "CA cert file should exist");
        assert!(key_path.exists(), "CA key file should exist");

        let cert_pem = fs::read_to_string(&cert_path).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));

        let key_pem = fs::read_to_string(&key_path).unwrap();
        assert!(key_pem.contains("BEGIN PRIVATE KEY") || key_pem.contains("BEGIN EC PRIVATE KEY"));
    }

    #[test]
    fn test_generate_ca_valid_x509() {
        let tmp = TempDir::new().unwrap();
        let (cert_path, _) = generate_ca(tmp.path()).unwrap();

        let pem_data = fs::read(&cert_path).unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(&pem_data).unwrap();
        let cert = pem.parse_x509().unwrap();

        assert!(cert.is_ca(), "CA cert should have CA basic constraint");
    }

    #[test]
    fn test_generate_server_cert_creates_files() {
        let tmp = TempDir::new().unwrap();
        generate_ca(tmp.path()).unwrap();
        let (cert_path, key_path) = generate_server_cert(tmp.path()).unwrap();

        assert!(cert_path.exists(), "server cert file should exist");
        assert!(key_path.exists(), "server key file should exist");
    }

    #[test]
    fn test_generate_server_cert_has_san() {
        let tmp = TempDir::new().unwrap();
        generate_ca(tmp.path()).unwrap();
        let (cert_path, _) = generate_server_cert(tmp.path()).unwrap();

        let pem_data = fs::read(&cert_path).unwrap();
        let (_, pem) = x509_parser::pem::parse_x509_pem(&pem_data).unwrap();
        let cert = pem.parse_x509().unwrap();

        let san = cert
            .extensions()
            .iter()
            .find(|ext| ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME);
        assert!(san.is_some(), "server cert should have SAN extension");
    }

    #[test]
    fn test_generate_host_cert_creates_files() {
        let tmp = TempDir::new().unwrap();
        generate_ca(tmp.path()).unwrap();
        let (cert_path, key_path) = generate_host_cert(tmp.path(), "myapp.localhost").unwrap();

        assert!(cert_path.exists(), "host cert file should exist");
        assert!(key_path.exists(), "host key file should exist");

        assert!(
            cert_path.parent().unwrap().ends_with(HOST_CERTS_DIR),
            "cert should be in host-certs directory"
        );
    }

    #[test]
    fn test_generate_host_cert_disk_caching() {
        let tmp = TempDir::new().unwrap();
        generate_ca(tmp.path()).unwrap();

        let (p1, _) = generate_host_cert(tmp.path(), "myapp.localhost").unwrap();
        let (p2, _) = generate_host_cert(tmp.path(), "myapp.localhost").unwrap();
        assert_eq!(p1, p2, "same hostname should produce same path");
    }
}
