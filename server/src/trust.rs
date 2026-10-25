//! The local CA as a trusted root, which Ubisoft Connect needs (it checks certificates through the OS).
//!
//! Used by `powder-server admin trust`. Only the CA certificate is installed (never a key); it is
//! name-constrained to the Ubisoft domains, see tls.rs.
//!
//! - Windows: the current user's Root store via certutil (no admin rights; Windows asks the user to confirm).
//! - Linux (Wine, Lutris, Proton): Wine's crypt32 fills its Root store from the host's CA bundle, so the CA goes
//!   into the system trust (p11-kit `trust anchor`, as root). Without root, the error names the command.

use crate::tls::{CA_NAME, LEGACY_CA_NAMES};
use anyhow::{anyhow, bail, Context, Result};
use sha1::{Digest, Sha1};
use std::path::Path;

/// The CA certificate (DER) in a PEM file.
fn ca_der(ca_pem: &Path) -> Result<Vec<u8>> {
    let pem = std::fs::read(ca_pem).with_context(|| format!("reading {}", ca_pem.display()))?;
    let cert = rustls_pemfile::certs(&mut pem.as_slice()).next()
        .ok_or_else(|| anyhow!("no certificate in {}", ca_pem.display()))??;
    Ok(cert.to_vec())
}

/// SHA-1 thumbprint as Windows shows it.
pub fn thumbprint(ca_pem: &Path) -> Result<String> {
    Ok(hex::encode_upper(Sha1::digest(ca_der(ca_pem)?)))
}

/// (SHA-1 thumbprint, subject) per certificate in `certutil -store` output.
pub fn parse_store(out: &str) -> Vec<(String, String)> {
    let block_sep = regex::Regex::new(r"=+ Certificate \d+ =+").unwrap();
    let subject = regex::Regex::new(r"(?m)^Subject: (.*)$").unwrap();
    let hash = regex::Regex::new(r"Cert Hash\(sha1\): ([0-9a-fA-F ]+)").unwrap();
    block_sep.split(out).skip(1).filter_map(|block| {
        let s = subject.captures(block)?[1].trim().to_string();
        let t = hash.captures(block)?[1].replace(' ', "").to_uppercase();
        Some((t, s))
    }).collect()
}

/// Is this CA a trusted root?
pub fn is_trusted(ca_pem: &Path) -> Result<bool> {
    imp::is_trusted(ca_pem)
}

/// Make the CA a trusted root (Windows asks the user to confirm; declining is an error).
pub fn install(ca_pem: &Path) -> Result<()> {
    imp::install(ca_pem)?;
    if !is_trusted(ca_pem)? {
        bail!("certificate not installed");
    }
    Ok(())
}

/// Remove the CA from the trusted roots, if it is there.
pub fn remove(ca_pem: &Path) -> Result<()> {
    if !is_trusted(ca_pem)? {
        return Ok(());
    }
    imp::remove(ca_pem)?;
    if is_trusted(ca_pem)? {
        bail!("certificate not removed");
    }
    Ok(())
}

/// Trusted roots from capture tooling that should not linger (e.g. mitmproxy).
pub fn other_tool_roots() -> Vec<String> {
    let subjects = imp::subjects().unwrap_or_default();
    let mut found = vec![];
    if subjects.iter().any(|s| s.contains("CN=mitmproxy")) {
        found.push("mitmproxy".to_string());
    }
    found
}

/// Every trusted root issued by a Powder server (any data folder's CA has one of these names).
pub fn installed_cas() -> Vec<String> {
    let names: Vec<&str> = std::iter::once(CA_NAME).chain(LEGACY_CA_NAMES.iter().copied()).collect();
    imp::subjects().unwrap_or_default().into_iter()
        .filter(|s| names.iter().any(|n| s.contains(&format!("CN={n}")))).collect()
}

/// A Wine prefix's own root store. Wine copies the host's CA bundle into the prefix's registry when the prefix is
/// set up (HKLM\Software\Microsoft\SystemCertificates\Root\Certificates\<SHA-1>), and crypt32 checks against
/// that copy: a CA added to the system later is not seen by an existing prefix (seen with GE-Proton under
/// Lutris: the TLS handshake completes, then Ubisoft Connect drops the connection). So the CA goes into the
/// prefix itself, through the prefix's own `wine reg`.
pub mod wine {
    use super::*;
    use std::process::Command;

    fn key(ca_pem: &Path) -> Result<String> {
        Ok(format!(r"HKEY_LOCAL_MACHINE\Software\Microsoft\SystemCertificates\Root\Certificates\{}", thumbprint(ca_pem)?))
    }

    /// The registry value Wine (and Windows) store a certificate as: properties (id, 1, length, data); here the
    /// SHA-1 hash (3) and the certificate (0x20), as in the entries Wine writes itself.
    pub fn blob(ca_der: &[u8]) -> Vec<u8> {
        let prop = |id: u32, data: &[u8]| -> Vec<u8> {
            [&id.to_le_bytes()[..], &1u32.to_le_bytes(), &(data.len() as u32).to_le_bytes(), data].concat()
        };
        [prop(3, &Sha1::digest(ca_der)), prop(0x20, ca_der)].concat()
    }

    /// A .reg file adding the CA to the prefix's trusted roots.
    pub fn reg_file(ca_pem: &Path) -> Result<String> {
        let hex: Vec<String> = blob(&ca_der(ca_pem)?).iter().map(|b| format!("{b:02x}")).collect();
        let lines: Vec<String> = hex.chunks(25).map(|c| c.join(",")).collect();
        Ok(format!("Windows Registry Editor Version 5.00\r\n\r\n[{}]\r\n\"Blob\"=hex:{}\r\n", key(ca_pem)?,
                   lines.join(",\\\r\n  ")))
    }

    fn reg(wine: &str, prefix: &Path, args: &[&str]) -> Result<std::process::Output> {
        Command::new(wine).arg("reg").args(args).env("WINEPREFIX", prefix).env("WINEDEBUG", "-all").output()
            .with_context(|| format!("running {wine} (use --wine with the Wine/Proton build the prefix runs with)"))
    }

    pub fn is_trusted(ca_pem: &Path, prefix: &Path, wine: &str) -> Result<bool> {
        Ok(reg(wine, prefix, &["query", &key(ca_pem)?])?.status.success())
    }

    /// Restart the prefix's programs (Ubisoft Connect) afterwards: crypt32 reads the store when it starts.
    pub fn install(ca_pem: &Path, prefix: &Path, wine: &str) -> Result<()> {
        let file = std::env::temp_dir().join(format!("powder-ca-{}.reg", std::process::id()));
        std::fs::write(&file, reg_file(ca_pem)?)?;
        let out = reg(wine, prefix, &["import", &file.to_string_lossy()]);
        let _ = std::fs::remove_file(&file);
        let out = out?;
        if !out.status.success() || !is_trusted(ca_pem, prefix, wine)? {
            bail!("certificate not added to {} ({})", prefix.display(), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    pub fn remove(ca_pem: &Path, prefix: &Path, wine: &str) -> Result<()> {
        if is_trusted(ca_pem, prefix, wine)? {
            reg(wine, prefix, &["delete", &key(ca_pem)?, "/f"])?;
            if is_trusted(ca_pem, prefix, wine)? {
                bail!("certificate not removed from {}", prefix.display());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Output};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn certutil(args: &[&str]) -> Result<Output> {
        Command::new("certutil").arg("-user").args(args).creation_flags(CREATE_NO_WINDOW).output()
            .context("running certutil")
    }

    fn store() -> Result<Vec<(String, String)>> {
        Ok(parse_store(&String::from_utf8_lossy(&certutil(&["-store", "Root"])?.stdout)))
    }

    pub fn subjects() -> Result<Vec<String>> {
        Ok(store()?.into_iter().map(|(_, s)| s).collect())
    }

    pub fn is_trusted(ca_pem: &Path) -> Result<bool> {
        let thumb = thumbprint(ca_pem)?;
        Ok(store()?.iter().any(|(t, _)| *t == thumb))
    }

    pub fn install(ca_pem: &Path) -> Result<()> {
        let out = certutil(&["-addstore", "Root", &ca_pem.to_string_lossy()])?;
        if !out.status.success() {
            bail!("certificate not installed (certutil exit {:?})", out.status.code());
        }
        Ok(())
    }

    pub fn remove(ca_pem: &Path) -> Result<()> {
        let out = certutil(&["-delstore", "Root", &thumbprint(ca_pem)?])?;
        if !out.status.success() {
            bail!("certificate not removed (certutil exit {:?})", out.status.code());
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::process::Command;

    /// Where distributions keep the combined CA bundle (the files Wine reads too).
    const BUNDLES: &[&str] = &[
        "/etc/ssl/certs/ca-certificates.crt", // Arch, Debian/Ubuntu, Steam Deck
        "/etc/pki/tls/certs/ca-bundle.crt",   // Fedora
        "/etc/ssl/ca-bundle.pem",             // openSUSE
        "/etc/ssl/cert.pem",
    ];

    fn bundle() -> Result<Vec<Vec<u8>>> {
        let path = BUNDLES.iter().map(Path::new).find(|p| p.is_file())
            .ok_or_else(|| anyhow!("no system CA bundle found (looked in {})", BUNDLES.join(", ")))?;
        let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(rustls_pemfile::certs(&mut pem.as_slice()).filter_map(|c| c.ok()).map(|c| c.to_vec()).collect())
    }

    pub fn subjects() -> Result<Vec<String>> {
        Ok(bundle()?.iter().filter_map(|der| {
            let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
            Some(cert.subject().to_string())
        }).collect())
    }

    pub fn is_trusted(ca_pem: &Path) -> Result<bool> {
        let ours = ca_der(ca_pem)?;
        Ok(bundle()?.contains(&ours))
    }

    fn trust_anchor(flag: Option<&str>, ca_pem: &Path) -> Result<()> {
        let ca = ca_pem.canonicalize().unwrap_or_else(|_| ca_pem.to_path_buf());
        let mut cmd = Command::new("trust");
        cmd.arg("anchor");
        if let Some(f) = flag {
            cmd.arg(f);
        }
        let shown = format!("sudo trust anchor {}{}", flag.map(|f| format!("{f} ")).unwrap_or_default(), ca.display());
        let out = match cmd.arg(&ca).output() {
            Ok(o) => o,
            Err(e) => bail!("`trust` (p11-kit) not available ({e}); add {} to the system CA store by hand \
                             (Debian/Ubuntu: copy it to /usr/local/share/ca-certificates/powder-ca.crt, then \
                             sudo update-ca-certificates)", ca.display()),
        };
        if !out.status.success() {
            bail!("{} (needs root: run `{shown}`)", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    pub fn install(ca_pem: &Path) -> Result<()> {
        trust_anchor(None, ca_pem)
    }

    pub fn remove(ca_pem: &Path) -> Result<()> {
        trust_anchor(Some("--remove"), ca_pem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certutil_store_output() {
        let out = "Root\n================ Certificate 0 ================\nSerial Number: 01\n\
                   Issuer: CN=Powder (Steep) local CA, O=Powder (Steep)\n\
                   Subject: CN=Powder (Steep) local CA, O=Powder (Steep)\n\
                   Cert Hash(sha1): ab cd ef 01\n\
                   ================ Certificate 1 ================\nSubject: CN=Other\nCert Hash(sha1): 12 34\n";
        assert_eq!(parse_store(out), vec![
            ("ABCDEF01".to_string(), "CN=Powder (Steep) local CA, O=Powder (Steep)".to_string()),
            ("1234".to_string(), "CN=Other".to_string()),
        ]);
    }

    #[test]
    fn wine_blob_has_the_layout_wine_writes() {
        let der = b"\x30\x03\x02\x01\x05";
        let b = wine::blob(der);
        assert_eq!(&b[..12], &[3, 0, 0, 0, 1, 0, 0, 0, 20, 0, 0, 0]);
        assert_eq!(&b[12..32], Sha1::digest(der).as_slice());
        assert_eq!(&b[32..44], &[0x20, 0, 0, 0, 1, 0, 0, 0, 5, 0, 0, 0]);
        assert_eq!(&b[44..], der);
    }

    #[test]
    fn thumbprint_matches_the_pki() {
        let dir = tempfile::tempdir().unwrap();
        let pki = crate::tls::ensure_pki(dir.path()).unwrap();
        assert_eq!(thumbprint(&pki.ca_pem).unwrap(), pki.ca_thumbprint().unwrap());
    }
}
