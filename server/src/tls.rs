//! TLS for the HTTPS and demux servers: a local CA, a server certificate, and the CA's CRL.
//!
//! The game accepts any certificate (in-process hook), but Ubisoft Connect checks certificates through
//! Windows, so it needs a chain to a root it trusts plus revocation information:
//!
//! - **local CA** (`ca.pem` + `ca-key.pem`): restricted by NameConstraints to the Ubisoft domains and
//!   loopback, so trusting it (`powder-server admin trust install`) cannot let it vouch for any other site,
//!   even if its key leaked.
//! - **server certificate** (`server.pem` = key + certificate + CA): all Ubisoft hostnames we answer,
//!   with a CRL distribution point on this machine.
//! - **CRL**: empty, signed by the CA, served over plain HTTP (`crl_url()`); Windows refuses a certificate
//!   whose revocation status it cannot check.
//!
//! Everything is generated on first run into `data/tls/` and never committed. Files, key types (ECDSA P-256,
//! SHA-256), names and key identifiers match what the Python server wrote, so an existing `data/tls/` (and
//! the CA users already trust) keeps working.

use crate::constants::{cert_names, CRL_HOST};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CertificateRevocationListParams, CidrSubnet, CrlDistributionPoint,
    CustomExtension, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, KeyIdMethod, KeyPair,
    KeyUsagePurpose, NameConstraints, SanType, SerialNumber, PKCS_ECDSA_P256_SHA256,
};
use rand::RngCore;
use sha1::{Digest, Sha1};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::rustls;
use x509_parser::extensions::{DistributionPointName, GeneralName, ParsedExtension};
use x509_parser::prelude::{parse_x509_certificate, X509Certificate};
use x509_parser::time::ASN1Time;

pub const PERMITTED_DOMAINS: &[&str] = &["ubi.com", "ubisoft.com"]; // NameConstraints: these and subdomains
pub const CRL_PATH: &str = "/powder-ca.crl";
pub const LEGACY_CRL_PATHS: &[&str] = &["/steep-ca.crl"]; // still served (before the rename to Powder)
pub const CA_NAME: &str = "Powder (Steep) local CA"; // what users see in Windows' certificate manager
pub const LEGACY_CA_NAMES: &[&str] = &["steepserver local CA"]; // CAs generated before the rename keep their name
pub const SERVER_ORG: &str = "Powder (Steep)";

const CA_LIFETIME_DAYS: i64 = 3650;
const SERVER_LIFETIME_DAYS: i64 = 825;
const CRL_LIFETIME_DAYS: i64 = 7;
const SERVER_RENEW_DAYS: i64 = 30; // a server certificate this close to expiry is replaced

/// Plain HTTP on port 80 of the server: a redirected hostname, so it works wherever the server runs.
pub fn crl_url() -> String {
    format!("http://{CRL_HOST}{CRL_PATH}")
}

pub struct Pki {
    pub dir: PathBuf,
    pub ca_pem: PathBuf,     // what `admin trust install` adds to Windows
    pub ca_key_pem: PathBuf,
    pub server_pem: PathBuf, // key + server certificate + CA
    ca_der: Vec<u8>,
    ca: Certificate, // the CA's parameters as an issuer (subject name, key identifier, key usages)
    ca_key: KeyPair,
}

impl Pki {
    /// An empty CRL, freshly signed (DER). Cheap enough to make per request.
    pub fn crl(&self) -> Result<Vec<u8>> {
        let now = Utc::now();
        let params = CertificateRevocationListParams {
            this_update: asn1_time(now - Duration::days(1))?.to_datetime(),
            next_update: asn1_time(now + Duration::days(CRL_LIFETIME_DAYS))?.to_datetime(),
            crl_number: SerialNumber::from(now.timestamp() as u64),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
            key_identifier_method: self.ca.params().key_identifier_method.clone(),
        };
        let crl = params.signed_by(&self.ca, &self.ca_key).context("signing the CRL")?;
        Ok(crl.der().to_vec())
    }

    /// SHA-1 thumbprint as Windows shows it (certutil, Cert:\ drive).
    pub fn ca_thumbprint(&self) -> Result<String> {
        Ok(hex::encode_upper(Sha1::digest(&self.ca_der)))
    }

    /// rustls config for the HTTPS and demux listeners: TLS 1.2+, server.pem's chain, no client auth, HTTP/1.1.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let pem = std::fs::read(&self.server_pem).with_context(|| format!("reading {}", self.server_pem.display()))?;
        let certs = rustls_pemfile::certs(&mut pem.as_slice())
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("parsing certificates in {}", self.server_pem.display()))?;
        let key = rustls_pemfile::private_key(&mut pem.as_slice())
            .with_context(|| format!("parsing the key in {}", self.server_pem.display()))?
            .with_context(|| format!("no private key in {}", self.server_pem.display()))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
            .context("TLS protocol versions")?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("server certificate and key")?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

fn asn1_time(t: DateTime<Utc>) -> Result<ASN1Time> {
    ASN1Time::from_timestamp(t.timestamp()).context("certificate time out of range")
}

/// Positive, nonzero, 159 bits (like cryptography's random_serial_number).
fn random_serial() -> SerialNumber {
    let mut b = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut b);
    b[0] = (b[0] & 0x7f) | 0x40;
    SerialNumber::from_slice(&b)
}

/// RFC 5280 method 1: SHA-1 of the public key bits (what cryptography's `from_public_key` computes).
fn key_id(key: &KeyPair) -> KeyIdMethod {
    KeyIdMethod::PreSpecified(Sha1::digest(key.public_key_raw()).to_vec())
}

fn name(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, SERVER_ORG);
    dn
}

fn new_key() -> Result<KeyPair> {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating an EC P-256 key")
}

/// A new CA: self-signed, path length 0, only certificate and CRL signing, NameConstraints.
fn make_ca() -> Result<(Certificate, KeyPair)> {
    let key = new_key()?;
    let now = Utc::now();
    let mut p = CertificateParams::default();
    p.distinguished_name = name(CA_NAME);
    p.serial_number = Some(random_serial());
    p.not_before = asn1_time(now - Duration::days(1))?.to_datetime();
    p.not_after = asn1_time(now + Duration::days(CA_LIFETIME_DAYS))?.to_datetime();
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: PERMITTED_DOMAINS.iter().map(|d| GeneralSubtree::DnsName(d.to_string()))
            .chain([GeneralSubtree::IpAddress(CidrSubnet::V4([127, 0, 0, 0], [255, 0, 0, 0]))])
            .collect(),
        excluded_subtrees: Vec::new(),
    });
    p.key_identifier_method = key_id(&key);
    let cert = p.self_signed(&key).context("signing the CA certificate")?;
    Ok((cert, key))
}

/// A new server certificate for every name in `cert_names()` and 127.0.0.1, signed by the CA.
fn make_server(ca: &Certificate, ca_key: &KeyPair) -> Result<(Certificate, KeyPair)> {
    let key = new_key()?;
    let now = Utc::now();
    let mut p = CertificateParams::default();
    p.distinguished_name = name(cert_names()[0]);
    p.serial_number = Some(random_serial());
    p.not_before = asn1_time(now - Duration::days(1))?.to_datetime();
    p.not_after = asn1_time(now + Duration::days(SERVER_LIFETIME_DAYS))?.to_datetime();
    let mut sans = Vec::new();
    for h in cert_names() {
        sans.push(SanType::DnsName(h.try_into().with_context(|| format!("hostname {h}"))?));
    }
    sans.push(SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    p.subject_alt_names = sans;
    // rcgen's IsCa::ExplicitNoCa encodes cA=FALSE explicitly, which is not DER (a DEFAULT value): Python's
    // cryptography rejects such a certificate. Write BasicConstraints (critical, empty) and the SKI ourselves.
    p.is_ca = IsCa::NoCa;
    let mut basic = CustomExtension::from_oid_content(&[2, 5, 29, 19], vec![0x30, 0x00]);
    basic.set_criticality(true);
    let ski = Sha1::digest(key.public_key_raw());
    let ski = CustomExtension::from_oid_content(&[2, 5, 29, 14], [&[0x04, ski.len() as u8][..], &ski].concat());
    p.custom_extensions = vec![basic, ski];
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.crl_distribution_points = vec![CrlDistributionPoint { uris: vec![crl_url()] }];
    p.use_authority_key_identifier_extension = true;
    let cert = p.signed_by(&key, ca, ca_key).context("signing the server certificate")?;
    Ok((cert, key))
}

/// The CA as an issuer: its parameters (names, key identifier) from the stored certificate, and its key.
fn load_ca(ca_pem: &Path, ca_key_pem: &Path) -> Result<(Vec<u8>, Certificate, KeyPair)> {
    let pem = std::fs::read(ca_pem).with_context(|| format!("reading {}", ca_pem.display()))?;
    let der = rustls_pemfile::certs(&mut pem.as_slice())
        .next()
        .with_context(|| format!("no certificate in {}", ca_pem.display()))?
        .with_context(|| format!("parsing {}", ca_pem.display()))?;
    let key_pem = std::fs::read_to_string(ca_key_pem).with_context(|| format!("reading {}", ca_key_pem.display()))?;
    let key = KeyPair::from_pem(&key_pem).with_context(|| format!("parsing the key in {}", ca_key_pem.display()))?;
    let (_, parsed) = parse_x509_certificate(&der).with_context(|| format!("parsing {}", ca_pem.display()))?;
    if parsed.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        bail!("{} does not belong to {}", ca_key_pem.display(), ca_pem.display());
    }
    let params = CertificateParams::from_ca_cert_der(&der).with_context(|| format!("reading {}", ca_pem.display()))?;
    // re-signing only rebuilds rcgen's issuer view; the stored ca.pem stays as it is
    let ca = params.self_signed(&key).context("loading the CA")?;
    Ok((der.to_vec(), ca, key))
}

/// An existing server.pem is reused only if this CA signed it, it is not about to expire, and it has the
/// current names and CRL point. (Earlier versions' certificates are replaced automatically.)
fn server_ok(server_pem: &Path, ca_der: &[u8]) -> bool {
    let Ok(pem) = std::fs::read(server_pem) else { return false };
    let Some(Ok(der)) = rustls_pemfile::certs(&mut pem.as_slice()).next() else { return false };
    let (Ok((_, cert)), Ok((_, ca))) = (parse_x509_certificate(&der), parse_x509_certificate(ca_der)) else {
        return false;
    };
    let renew_at = Utc::now() + Duration::days(SERVER_RENEW_DAYS);
    // the signature check also catches a CA regenerated under the same name (the Python check compared names)
    cert.issuer().as_raw() == ca.subject().as_raw()
        && cert.verify_signature(Some(ca.public_key())).is_ok()
        && cert.subject().as_raw() != cert.issuer().as_raw()
        && cert.validity().not_after.timestamp() > renew_at.timestamp()
        && dns_names(&cert).is_some_and(|names| cert_names().iter().all(|h| names.contains(h)))
        && crl_points(&cert).is_some_and(|points| points == [crl_url()])
}

fn dns_names<'a>(cert: &'a X509Certificate) -> Option<Vec<&'a str>> {
    let san = cert.subject_alternative_name().ok()??;
    Some(san.value.general_names.iter().filter_map(|n| match n {
        GeneralName::DNSName(d) => Some(*d),
        _ => None,
    }).collect())
}

/// The first full-name URI of each CRL distribution point (None without the extension).
fn crl_points(cert: &X509Certificate) -> Option<Vec<String>> {
    cert.extensions().iter().find_map(|ext| match ext.parsed_extension() {
        ParsedExtension::CRLDistributionPoints(cdp) => Some(cdp.points.iter().map(|p| match &p.distribution_point {
            Some(DistributionPointName::FullName(names)) => match names.first() {
                Some(GeneralName::URI(u)) => u.to_string(),
                _ => String::new(),
            },
            _ => String::new(),
        }).collect()),
        _ => None,
    })
}

/// Load the CA and server certificate from `tls_dir`, creating whatever is missing or outdated.
pub fn ensure_pki(tls_dir: &Path) -> Result<Pki> {
    std::fs::create_dir_all(tls_dir).with_context(|| format!("creating {}", tls_dir.display()))?;
    let (ca_pem, ca_key_pem, server_pem) =
        (tls_dir.join("ca.pem"), tls_dir.join("ca-key.pem"), tls_dir.join("server.pem"));
    let (ca_der, ca, ca_key) = if ca_pem.exists() && ca_key_pem.exists() {
        load_ca(&ca_pem, &ca_key_pem)?
    } else {
        let (ca, ca_key) = make_ca()?;
        write(&ca_key_pem, ca_key.serialize_pem().as_bytes())?;
        write(&ca_pem, ca.pem().as_bytes())?;
        log::info!(target: "powder", "generated local CA {} (install it for Ubisoft Connect: powder-server admin \
                                      trust install)", ca_pem.display());
        (ca.der().to_vec(), ca, ca_key)
    };
    if !server_ok(&server_pem, &ca_der) {
        let (cert, key) = make_server(&ca, &ca_key)?;
        let ca_text = pem_cert(&ca_der);
        write(&server_pem, format!("{}{}{}", key.serialize_pem(), cert.pem(), ca_text).as_bytes())?;
        log::info!(target: "powder", "generated server certificate {}", server_pem.display());
    }
    Ok(Pki { dir: tls_dir.to_path_buf(), ca_pem, ca_key_pem, server_pem, ca_der, ca, ca_key })
}

/// The stored CA certificate as PEM (the original bytes, not rcgen's re-signed copy).
fn pem_cert(der: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::parse_x509_crl;

    fn certs(path: &Path) -> Vec<Vec<u8>> {
        let pem = std::fs::read(path).unwrap();
        rustls_pemfile::certs(&mut pem.as_slice()).map(|c| c.unwrap().to_vec()).collect()
    }

    #[test]
    fn creates_then_reloads_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");
        let first = ensure_pki(&tls).unwrap();
        let files = ["ca.pem", "ca-key.pem", "server.pem"].map(|f| std::fs::read(tls.join(f)).unwrap());
        let second = ensure_pki(&tls).unwrap();
        assert_eq!(files, ["ca.pem", "ca-key.pem", "server.pem"].map(|f| std::fs::read(tls.join(f)).unwrap()));
        assert_eq!(first.ca_thumbprint().unwrap(), second.ca_thumbprint().unwrap());
        assert_eq!(second.ca_thumbprint().unwrap().len(), 40);
        assert_eq!(second.dir, tls);
    }

    #[test]
    fn server_cert_chains_to_ca_with_expected_names() {
        let dir = tempfile::tempdir().unwrap();
        let pki = ensure_pki(dir.path()).unwrap();
        let chain = certs(&pki.server_pem);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[1], certs(&pki.ca_pem)[0]);
        let (_, server) = parse_x509_certificate(&chain[0]).unwrap();
        let (_, ca) = parse_x509_certificate(&chain[1]).unwrap();
        server.verify_signature(Some(ca.public_key())).unwrap();
        ca.verify_signature(None).unwrap();
        assert_eq!(server.issuer(), ca.subject());
        let names = dns_names(&server).unwrap();
        assert!(cert_names().iter().all(|h| names.contains(h)));
        assert!(names.contains(&crate::constants::DEMUX_HOST));
        assert!(crate::constants::redirect_hosts().iter().all(|h| names.contains(h)));
        assert_eq!(crl_points(&server).unwrap(), [crl_url()]);
        // key identifiers: the server's AKI is the CA's SKI (SHA-1 of the public key bits)
        let ski = |c: &X509Certificate| c.extensions().iter().find_map(|e| match e.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
            _ => None,
        }).unwrap();
        let aki = server.extensions().iter().find_map(|e| match e.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(a) => a.key_identifier.as_ref().map(|k| k.0.to_vec()),
            _ => None,
        }).unwrap();
        assert_eq!(aki, ski(&ca));
        assert_eq!(ski(&ca), Sha1::digest(ca.public_key().subject_public_key.data.as_ref()).to_vec());
        assert_eq!(ski(&server), Sha1::digest(server.public_key().subject_public_key.data.as_ref()).to_vec());
        assert!(ca.is_ca() && !server.is_ca());
        // BasicConstraints present, critical, and DER-clean (an explicit cA=FALSE makes cryptography refuse it)
        let basic = server.basic_constraints().unwrap().unwrap();
        assert!(basic.critical && !basic.value.ca);
        let ext = server.extensions().iter().find(|e| e.oid.to_id_string() == "2.5.29.19").unwrap();
        assert_eq!(ext.value, [0x30, 0x00]);
        assert!(ca.name_constraints().unwrap().is_some());
    }

    #[test]
    fn outdated_server_cert_is_replaced_but_ca_kept() {
        let dir = tempfile::tempdir().unwrap();
        let pki = ensure_pki(dir.path()).unwrap();
        let ca_before = std::fs::read(&pki.ca_pem).unwrap();
        // a server.pem from another CA: replaced
        let (other_ca, other_key) = make_ca().unwrap();
        let (cert, key) = make_server(&other_ca, &other_key).unwrap();
        std::fs::write(&pki.server_pem, format!("{}{}", key.serialize_pem(), cert.pem())).unwrap();
        assert!(!server_ok(&pki.server_pem, &pki.ca_der));
        let pki = ensure_pki(dir.path()).unwrap();
        assert!(server_ok(&pki.server_pem, &pki.ca_der));
        assert_eq!(std::fs::read(&pki.ca_pem).unwrap(), ca_before);
    }

    #[test]
    fn server_config_builds() {
        let dir = tempfile::tempdir().unwrap();
        let config = ensure_pki(dir.path()).unwrap().server_config().unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn crl_parses_and_is_signed_by_ca() {
        let dir = tempfile::tempdir().unwrap();
        let pki = ensure_pki(dir.path()).unwrap();
        let der = pki.crl().unwrap();
        let (_, crl) = parse_x509_crl(&der).unwrap();
        let (_, ca) = parse_x509_certificate(&pki.ca_der).unwrap();
        assert_eq!(crl.issuer(), ca.subject());
        assert_eq!(crl.iter_revoked_certificates().count(), 0);
        assert!(crl.crl_number().is_some());
        assert!(crl.next_update().unwrap().timestamp() > Utc::now().timestamp());
        crl.verify_signature(ca.public_key()).unwrap();
    }
}
