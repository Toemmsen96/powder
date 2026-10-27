"""The server's CA must be trusted by Windows for Ubisoft Connect. We fetch the CA certificate from the
server (/_powder/ca.pem; works the same for a local or remote server), check it, keep a copy, and install it
into the current user's trusted roots when asked (Windows shows its own confirmation).

Whoever runs a server holds its CA's key, and a trusted root vouches for whatever it signs, so the launcher
only lets Windows trust a CA that cannot vouch for anything but the Ubisoft names: check_ca() takes exactly
one self-signed certificate, BasicConstraints CA (critical, path length 0), and critical NameConstraints that
permit only DNS names within ubi.com / ubisoft.com (the Python server's CAs permit those two domains, the Rust
server's new ones the redirected hostnames) and IPv4 addresses within 127.0.0.0/8, with no other name forms
and no excluded subtrees. Anything else (another CA, a bundle, a key) is refused before Windows sees it, and
only the re-serialized certificate is written for certutil.
"""
from __future__ import annotations

import ipaddress
import re
from pathlib import Path

from cryptography import x509
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.x509.oid import ExtendedKeyUsageOID

from powder.server import trust

EXPLANATION = (
    "Ubisoft Connect only talks to servers whose certificate Windows trusts. The server has its own small "
    "certificate authority; the launcher checked that it is technically limited to Ubisoft's domains, so it "
    "cannot vouch for any other website. It is added for your Windows user only (no admin rights). When you "
    "click \"Install certificate\", Windows shows a security warning asking you to confirm it; answer Yes. "
    "Restore system removes it again.")

MAX_PEM = 64 << 10                                   # bytes of /_powder/ca.pem read at most
ALLOWED_DOMAINS = ("ubi.com", "ubisoft.com")         # permitted DNS subtrees must lie within these
ALLOWED_NET = ipaddress.ip_network("127.0.0.0/8")    # permitted IP subtrees must lie within this
ALLOWED_EKU = {ExtendedKeyUsageOID.SERVER_AUTH, ExtendedKeyUsageOID.CLIENT_AUTH}
_LABEL = r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
HOSTNAME = re.compile(rf"{_LABEL}(?:\.{_LABEL})*")
FINGERPRINT = re.compile(r"[0-9a-f]{16,64}")       # what a Join address may carry after "#" (a prefix is fine)


class CaError(ValueError):
    """The server's CA is refused; the text says why, for the player."""


def _ext(cert: x509.Certificate, cls):
    try:
        return cert.extensions.get_extension_for_class(cls)
    except x509.ExtensionNotFound:
        return None


def _dns_ok(name: str) -> bool:
    name = name.lower().removeprefix(".")             # ".ubi.com" (subdomains only) is narrower still
    return bool(HOSTNAME.fullmatch(name)) and any(name == d or name.endswith("." + d) for d in ALLOWED_DOMAINS)


def check_ca(pem: bytes) -> x509.Certificate:
    """The CA certificate in `pem` if Windows may trust it (see the module doc); CaError otherwise."""
    if len(pem) > MAX_PEM:
        raise CaError("it is larger than a CA certificate")
    try:
        text = pem.decode("ascii")
    except UnicodeDecodeError as e:
        raise CaError("it is not a PEM certificate") from e
    if text.count("-----BEGIN ") != 1 or text.count("-----BEGIN CERTIFICATE-----") != 1:
        raise CaError("it must be exactly one certificate (no bundle, no key)")
    try:
        cert = x509.load_pem_x509_certificate(pem)
    except ValueError as e:
        raise CaError(f"it is not a valid certificate ({e})") from e
    try:
        cert.extensions
    except ValueError as e:
        raise CaError(f"its extensions can't be read ({e})") from e
    if cert.issuer != cert.subject:
        raise CaError("it is not self-signed")
    try:
        cert.verify_directly_issued_by(cert)
    except (ValueError, TypeError, InvalidSignature) as e:
        raise CaError("its self-signature does not verify") from e
    bc = _ext(cert, x509.BasicConstraints)
    if not bc or not bc.critical or not bc.value.ca or bc.value.path_length != 0:
        raise CaError("it is not a CA limited to signing server certificates directly (critical BasicConstraints, "
                      "path length 0)")
    ku = _ext(cert, x509.KeyUsage)
    if ku and not ku.value.key_cert_sign:
        raise CaError("its key usage does not allow signing certificates")
    eku = _ext(cert, x509.ExtendedKeyUsage)
    if eku and not set(eku.value) <= ALLOWED_EKU:
        raise CaError("it is allowed for more than TLS (extended key usage)")
    nc = _ext(cert, x509.NameConstraints)
    if not nc or not nc.critical:
        raise CaError("it is not limited to Ubisoft's names (no critical NameConstraints): it could vouch for any "
                      "website")
    if nc.value.excluded_subtrees:
        raise CaError("its NameConstraints have excluded subtrees (Powder's CAs have none)")
    permitted = list(nc.value.permitted_subtrees or [])
    dns = [g.value for g in permitted if isinstance(g, x509.DNSName)]
    ips = [g.value for g in permitted if isinstance(g, x509.IPAddress)]
    if len(dns) + len(ips) != len(permitted):
        raise CaError("its NameConstraints permit other kinds of names than DNS names and IP addresses")
    if not dns or not ips:
        raise CaError("its NameConstraints leave DNS names or IP addresses unconstrained")
    if bad := [d for d in dns if not _dns_ok(d)]:
        raise CaError(f"it may vouch for names outside ubi.com and ubisoft.com "
                      f"({', '.join(repr(d)[:60] for d in bad[:3])})")
    if bad := [n for n in ips if not (isinstance(n, ipaddress.IPv4Network) and n.subnet_of(ALLOWED_NET))]:
        raise CaError(f"it may vouch for addresses outside 127.0.0.0/8 ({', '.join(map(str, bad[:3]))})")
    return cert


def sha256(cert: x509.Certificate) -> str:
    """The CA's fingerprint (SHA-256 of the certificate, lower-case hex): what the launcher pins per server."""
    return cert.fingerprint(hashes.SHA256()).hex()


def thumbprint(cert: x509.Certificate) -> str:
    """SHA-1 thumbprint as Windows shows it (how certutil names it)."""
    return cert.fingerprint(hashes.SHA1()).hex().upper()


def leaf_tbs_pin(leaf_der: bytes) -> bytes:
    """The 32 bytes the TLS helper pins a server certificate by: SHA-256 of its tbsCertificate (the signed
    body). The game's OpenSSL caches those same bytes for the certificate it receives (hook_guard.c), so this
    matches only the exact certificate the server presents, and no other certificate for any host."""
    tbs = x509.load_der_x509_certificate(leaf_der).tbs_certificate_bytes
    d = hashes.Hash(hashes.SHA256())
    d.update(tbs)
    return d.finalize()


def pretty(fingerprint: str) -> str:
    """For people: upper-case hex in groups of four."""
    f = fingerprint.upper()
    return " ".join(f[i:i + 4] for i in range(0, len(f), 4))


def normalize_fingerprint(text: str) -> str:
    """A fingerprint as typed or pasted (spaces, colons, either case) as compared: lower-case hex."""
    return re.sub(r"[\s:]", "", text).lower()


def save_ca(cert: x509.Certificate, path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    return path


def load(path: Path) -> x509.Certificate | None:
    try:
        return x509.load_pem_x509_certificate(path.read_bytes())
    except (OSError, ValueError):
        return None


def is_trusted(ca_path: Path) -> bool:
    return trust.is_trusted(ca_path)


def install(ca_path: Path) -> None:
    trust.install(ca_path)
