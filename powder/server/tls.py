"""TLS for the HTTPS and demux servers: a local CA, a server certificate, and the CA's CRL.

The game accepts any certificate (in-process hook), but Ubisoft Connect checks certificates through
Windows, so it needs a chain to a root it trusts plus revocation information:

- **local CA** (`ca.pem` + `ca-key.pem`): restricted by NameConstraints to the hostnames the server is reached
  at (`REDIRECT_HOSTS`, each with its subdomains) and loopback, so trusting it (`powder-server admin trust
  install`) cannot let it vouch for any other site, even if its key leaked: not for the rest of Ubisoft's sites
  either (accounts, store). CAs made before 2026-10-25 permit all of ubi.com and ubisoft.com
  (`LEGACY_PERMITTED_DOMAINS`); they are kept as they are (players trust them), and a new CA is made only by
  deleting `data/tls/` (then trusted again on each PC).
- **server certificate** (`server.pem` = key + certificate + CA): the Ubisoft hostnames we answer that its CA
  permits (`server_names`: under a narrow CA without the `*.ubi.com`-style wildcards, which it would not
  allow: a certificate with one name its CA does not permit is refused as a whole), with a CRL distribution
  point on this machine.
- The private keys (`ca-key.pem`, `server.pem`) are written readable by their owner only (0600 on Unix), and an
  existing key file readable by others is made so when it is loaded.
- **CRL**: empty, signed by the CA, served over plain HTTP (`CRL_URL`); Windows refuses a certificate
  whose revocation status it cannot check.

Everything is generated on first run into `data/tls/` and never committed. The Rust server (rework branch)
writes and reads the same files the same way, so both can share a `data/tls/`.
"""
from __future__ import annotations

import datetime
import ipaddress
import logging
from dataclasses import dataclass
from pathlib import Path

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

from .hostnames import CERT_NAMES, CRL_HOST, REDIRECT_HOSTS
from .util import make_private, write_private

log = logging.getLogger("powder.tls")

HOSTNAMES = CERT_NAMES
CA_PERMITTED = list(REDIRECT_HOSTS)                     # a new CA's NameConstraints: these and their subdomains
LEGACY_PERMITTED_DOMAINS = ["ubi.com", "ubisoft.com"]   # those of the CAs made before 2026-10-25
PERMITTED_NET = ipaddress.ip_network("127.0.0.0/8")
CRL_PATH = "/powder-ca.crl"
CRL_URL = f"http://{CRL_HOST}{CRL_PATH}"                 # plain HTTP on port 80 of the server: a redirected
                                                         # hostname, so it works wherever the server runs
LEGACY_CRL_PATHS = ("/steep-ca.crl",)                    # still served (before the rename to Powder)
CA_NAME = "Powder (Steep) local CA"                      # what users see in Windows' certificate manager
LEGACY_CA_NAMES = ("steepserver local CA",)              # CAs generated before the rename keep their name
SERVER_ORG = "Powder (Steep)"

_DAY = datetime.timedelta(days=1)
CA_LIFETIME = 3650 * _DAY
SERVER_LIFETIME = 825 * _DAY
CRL_LIFETIME = 7 * _DAY


@dataclass
class Pki:
    ca_cert: x509.Certificate
    ca_key: ec.EllipticCurvePrivateKey
    server_pem: Path                                     # key + server certificate + CA (for ssl)
    ca_pem: Path                                         # what `admin trust install` adds to Windows

    def crl(self) -> bytes:
        """An empty CRL, freshly signed (DER). Cheap enough to make per request."""
        now = _now()
        crl = (x509.CertificateRevocationListBuilder()
               .issuer_name(self.ca_cert.subject)
               .last_update(now - _DAY)
               .next_update(now + CRL_LIFETIME)
               .add_extension(x509.CRLNumber(int(now.timestamp())), critical=False)
               .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(self.ca_key.public_key()),
                              critical=False)
               .sign(self.ca_key, hashes.SHA256()))
        return crl.public_bytes(serialization.Encoding.DER)

    @property
    def ca_thumbprint(self) -> str:
        """SHA-1 thumbprint as Windows shows it (certutil, Cert:\\ drive)."""
        return self.ca_cert.fingerprint(hashes.SHA1()).hex().upper()


def _now() -> datetime.datetime:
    return datetime.datetime.now(datetime.timezone.utc)


def _key_pem(key) -> bytes:
    return key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                             serialization.NoEncryption())


def _cert_pem(cert: x509.Certificate) -> bytes:
    return cert.public_bytes(serialization.Encoding.PEM)


def _make_ca(permitted=CA_PERMITTED) -> tuple[x509.Certificate, ec.EllipticCurvePrivateKey]:
    """A new CA: self-signed, path length 0, only certificate and CRL signing, NameConstraints: these DNS names
    (each with its subdomains; not CERT_NAMES' wildcards, `*.ubi.com` would permit all of ubi.com again) and
    127.0.0.0/8."""
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, CA_NAME),
                      x509.NameAttribute(NameOID.ORGANIZATION_NAME, SERVER_ORG)])
    now = _now()
    cert = (x509.CertificateBuilder()
            .subject_name(name).issuer_name(name)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - _DAY).not_valid_after(now + CA_LIFETIME)
            .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
            .add_extension(x509.KeyUsage(digital_signature=False, content_commitment=False,
                                         key_encipherment=False, data_encipherment=False,
                                         key_agreement=False, key_cert_sign=True, crl_sign=True,
                                         encipher_only=False, decipher_only=False), critical=True)
            .add_extension(x509.NameConstraints(
                permitted_subtrees=[x509.DNSName(d) for d in permitted]
                + [x509.IPAddress(PERMITTED_NET)],
                excluded_subtrees=None), critical=True)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
            .sign(key, hashes.SHA256()))
    return cert, key


def permitted_dns(ca_cert: x509.Certificate) -> list[str] | None:
    """The DNS names a CA's NameConstraints permit (each with its subdomains); None: no DNS constraint (all)."""
    try:
        nc = ca_cert.extensions.get_extension_for_class(x509.NameConstraints).value
    except x509.ExtensionNotFound:
        return None
    dns = [d.value.lstrip(".").lower() for d in nc.permitted_subtrees or () if isinstance(d, x509.DNSName)]
    return dns or None


def _within(name: str, permitted: list[str]) -> bool:
    """Whether `name` (a wildcard like `*.ubi.com` too) is inside one of these subtrees (RFC 5280: the name or a
    subdomain of it)."""
    name = name.lower()
    return any(name == p or name.endswith("." + p) for p in permitted)


def server_names(ca_cert: x509.Certificate) -> list[str]:
    """The CERT_NAMES a server certificate from this CA may carry. A certificate with a name its CA does not
    permit fails as a whole (Windows, OpenSSL, rustls), so the others are left out."""
    permitted = permitted_dns(ca_cert)
    return [n for n in HOSTNAMES if permitted is None or _within(n, permitted)]


def _make_server(ca_cert: x509.Certificate, ca_key,
                 names: list[str] | None = None) -> tuple[x509.Certificate, ec.EllipticCurvePrivateKey]:
    """A server certificate for these names (default: what the CA permits; the first is its subject) and
    127.0.0.1, signed by the CA."""
    names = server_names(ca_cert) if names is None else names
    if not names:
        raise ValueError("no hostnames for the server certificate (the CA permits none of them)")
    key = ec.generate_private_key(ec.SECP256R1())
    now = _now()
    cert = (x509.CertificateBuilder()
            .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, names[0]),
                                     x509.NameAttribute(NameOID.ORGANIZATION_NAME, SERVER_ORG)]))
            .issuer_name(ca_cert.subject)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - _DAY).not_valid_after(now + SERVER_LIFETIME)
            .add_extension(x509.SubjectAlternativeName(
                [x509.DNSName(h) for h in names] + [x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]),
                critical=False)
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .add_extension(x509.KeyUsage(digital_signature=True, content_commitment=False,
                                         key_encipherment=False, data_encipherment=False,
                                         key_agreement=False, key_cert_sign=False, crl_sign=False,
                                         encipher_only=False, decipher_only=False), critical=True)
            .add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
            .add_extension(x509.CRLDistributionPoints([x509.DistributionPoint(
                full_name=[x509.UniformResourceIdentifier(CRL_URL)], relative_name=None,
                reasons=None, crl_issuer=None)]), critical=False)
            .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
            .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()),
                           critical=False)
            .sign(ca_key, hashes.SHA256()))
    return cert, key


def _load_ca(ca_pem: Path, ca_key_pem: Path):
    if not (ca_pem.exists() and ca_key_pem.exists()):
        return None
    return (x509.load_pem_x509_certificate(ca_pem.read_bytes()),
            serialization.load_pem_private_key(ca_key_pem.read_bytes(), password=None))


def _server_ok(server_pem: Path, ca_cert: x509.Certificate) -> bool:
    """An existing server.pem is reused only if this CA issued it, it is not about to expire, and it has
    the names its CA permits (and no other: one would make clients refuse it, as a server.pem with wildcards under
    a narrow CA) and the CRL point. (Earlier versions' certificates are replaced automatically.)"""
    if not server_pem.exists():
        return False
    try:
        cert = x509.load_pem_x509_certificates(server_pem.read_bytes())[0]
        names = cert.extensions.get_extension_for_class(x509.SubjectAlternativeName).value
        cdp = cert.extensions.get_extension_for_class(x509.CRLDistributionPoints).value
    except (ValueError, x509.ExtensionNotFound):
        return False
    dns, permitted = names.get_values_for_type(x509.DNSName), permitted_dns(ca_cert)
    return (cert.issuer == ca_cert.subject and cert.subject != cert.issuer
            and cert.not_valid_after_utc - _now() > 30 * _DAY
            and set(server_names(ca_cert)) <= set(dns)
            and (permitted is None or all(_within(n, permitted) for n in dns))
            and [p.full_name[0].value for p in cdp] == [CRL_URL])


def ensure_pki(tls_dir: Path) -> Pki:
    """Load the CA and server certificate from `tls_dir`, creating whatever is missing or outdated."""
    tls_dir.mkdir(parents=True, exist_ok=True)
    ca_pem, ca_key_pem, server_pem = tls_dir / "ca.pem", tls_dir / "ca-key.pem", tls_dir / "server.pem"
    if ca_key_pem.exists():
        make_private(ca_key_pem, log)
    loaded = _load_ca(ca_pem, ca_key_pem)
    if loaded is None:
        ca_cert, ca_key = _make_ca()
        write_private(ca_key_pem, _key_pem(ca_key))
        ca_pem.write_bytes(_cert_pem(ca_cert))
        log.info("generated local CA %s (install it for Ubisoft Connect: powder-server admin trust install)",
                 ca_pem)
    else:
        ca_cert, ca_key = loaded
        permitted = permitted_dns(ca_cert)
        if permitted is not None and _within("any.ubi.com", permitted):
            log.info("the local CA permits all of ubi.com (made before 2026-10-25); a new one would permit only "
                     "the server's names (delete data/tls/, trust it again)")
    if server_pem.exists():
        make_private(server_pem, log)
    if not _server_ok(server_pem, ca_cert):
        cert, key = _make_server(ca_cert, ca_key)
        write_private(server_pem, _key_pem(key) + _cert_pem(cert) + _cert_pem(ca_cert))
        log.info("generated server certificate %s", server_pem)
    return Pki(ca_cert, ca_key, server_pem, ca_pem)
