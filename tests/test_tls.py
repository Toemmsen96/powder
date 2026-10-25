"""Local CA + server certificate + CRL (what Ubisoft Connect checks through Windows)."""
import datetime
import ipaddress
import os
import ssl

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

from powder.server import tls, util
from powder.server.hostnames import CERT_NAMES, REDIRECT_HOSTS
from powder.server.transport.nex import secure


def _server_cert(pki):
    return x509.load_pem_x509_certificates(pki.server_pem.read_bytes())[0]


def _dns(cert):
    return cert.extensions.get_extension_for_class(x509.SubjectAlternativeName).value.get_values_for_type(
        x509.DNSName)


def _handshake(tmp_path, chain_pem: bytes, ca_pem: bytes, sni: str) -> None:
    """A TLS handshake in memory (OpenSSL on both sides; it checks NameConstraints like Windows): the server
    presents `chain_pem` (key + certificates), the client trusts only `ca_pem`. Raises ssl.SSLError if refused."""
    (tmp_path / "chain.pem").write_bytes(chain_pem)
    (tmp_path / "trust.pem").write_bytes(ca_pem)
    server = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server.load_cert_chain(str(tmp_path / "chain.pem"))
    client = ssl.create_default_context(cafile=str(tmp_path / "trust.pem"))
    s_in, s_out, c_in, c_out = (ssl.MemoryBIO() for _ in range(4))
    s = server.wrap_bio(s_in, s_out, server_side=True)
    c = client.wrap_bio(c_in, c_out, server_hostname=sni)
    done = {s: False, c: False}
    for _ in range(20):
        for side in (c, s):
            if not done[side]:
                try:
                    side.do_handshake()
                    done[side] = True
                except ssl.SSLWantReadError:
                    pass
        if c_out.pending:
            s_in.write(c_out.read())
        if s_out.pending:
            c_in.write(s_out.read())
        if all(done.values()):
            return
    raise AssertionError("handshake did not finish")


def _chain(cert, key, ca_cert) -> bytes:
    return tls._key_pem(key) + tls._cert_pem(cert) + tls._cert_pem(ca_cert)


def test_chain_and_constraints(tmp_path):
    pki = tls.ensure_pki(tmp_path)
    ca, leaf = pki.ca_cert, _server_cert(pki)
    # leaf is signed by the CA
    ca.public_key().verify(leaf.signature, leaf.tbs_certificate_bytes, ec.ECDSA(hashes.SHA256()))
    assert leaf.issuer == ca.subject
    # a new CA may only vouch for the redirected hostnames (and their subdomains) and loopback
    nc = ca.extensions.get_extension_for_class(x509.NameConstraints)
    assert nc.critical
    assert set(nc.value.permitted_subtrees) == {x509.DNSName(h) for h in REDIRECT_HOSTS} | {
        x509.IPAddress(ipaddress.ip_network("127.0.0.0/8"))}
    assert tls.permitted_dns(ca) == REDIRECT_HOSTS
    bc = ca.extensions.get_extension_for_class(x509.BasicConstraints).value
    assert bc.ca and bc.path_length == 0
    assert _dns(leaf) == REDIRECT_HOSTS, "a new CA's server certificate: the redirected names only"
    assert "dmx.upc.ubisoft.com" in tls.HOSTNAMES
    cdp = leaf.extensions.get_extension_for_class(x509.CRLDistributionPoints).value
    assert cdp[0].full_name[0].value == tls.CRL_URL


def test_crl_is_signed_by_ca(tmp_path):
    pki = tls.ensure_pki(tmp_path)
    crl = x509.load_der_x509_crl(pki.crl())
    assert crl.is_signature_valid(pki.ca_cert.public_key())
    assert len(crl) == 0
    assert crl.next_update_utc > crl.last_update_utc


def test_reuse_and_replace_old_self_signed(tmp_path):
    pki = tls.ensure_pki(tmp_path)
    first = pki.server_pem.read_bytes()
    assert tls.ensure_pki(tmp_path).server_pem.read_bytes() == first          # reused
    # an old-style self-signed server.pem is replaced; the CA stays
    pki.server_pem.write_bytes(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
    again = tls.ensure_pki(tmp_path)
    assert again.ca_thumbprint == pki.ca_thumbprint
    assert _server_cert(again).issuer == pki.ca_cert.subject


def test_a_new_ca_vouches_only_for_the_server_names(tmp_path):
    """The server certificate of a narrow CA works for every redirected host; a certificate for any other Ubisoft
    site signed with its key (as by someone who read ca-key.pem) is refused by clients."""
    pki = tls.ensure_pki(tmp_path / "tls")
    ca_pem, server = pki.ca_pem.read_bytes(), pki.server_pem.read_bytes()
    for host in REDIRECT_HOSTS:
        _handshake(tmp_path, server, ca_pem, host)
    for other in ("account.ubisoft.com", "store.ubi.com", "connect.ubisoft.com"):
        cert, key = tls._make_server(pki.ca_cert, pki.ca_key, [other])
        with pytest.raises(ssl.SSLCertVerificationError, match="(?i)name constraint|permitted"):
            _handshake(tmp_path, _chain(cert, key, pki.ca_cert), ca_pem, other)


def test_a_narrow_ca_made_by_the_rust_server(tmp_path):
    """A CA as the Rust server makes it (NameConstraints: the redirected hostnames + 127.0.0.0/8; built here
    independently of tls._make_ca): kept, and the server certificate carries only the names it permits, also
    when an older server.pem with the wildcards (refused by every client under this CA) is there."""
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, tls.CA_NAME)])
    now = datetime.datetime.now(datetime.timezone.utc)
    ca = (x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
          .serial_number(x509.random_serial_number())
          .not_valid_before(now - datetime.timedelta(days=1)).not_valid_after(now + datetime.timedelta(days=3650))
          .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
          .add_extension(x509.KeyUsage(digital_signature=False, content_commitment=False, key_encipherment=False,
                                       data_encipherment=False, key_agreement=False, key_cert_sign=True,
                                       crl_sign=True, encipher_only=False, decipher_only=False), critical=True)
          .add_extension(x509.NameConstraints(
              permitted_subtrees=[x509.DNSName(h) for h in REDIRECT_HOSTS]
              + [x509.IPAddress(ipaddress.ip_network("127.0.0.0/8"))], excluded_subtrees=None), critical=True)
          .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
          .sign(key, hashes.SHA256()))
    d = tmp_path / "tls"
    d.mkdir()
    ca_pem = ca.public_bytes(serialization.Encoding.PEM)
    (d / "ca.pem").write_bytes(ca_pem)
    (d / "ca-key.pem").write_bytes(tls._key_pem(key))
    # what the Python server wrote before: every CERT_NAMES entry, wildcards included
    cert, leaf_key = tls._make_server(ca, key, list(CERT_NAMES))
    (d / "server.pem").write_bytes(_chain(cert, leaf_key, ca))
    with pytest.raises(ssl.SSLCertVerificationError, match="(?i)name constraint|permitted"):
        _handshake(tmp_path, (d / "server.pem").read_bytes(), ca_pem, REDIRECT_HOSTS[0])
    assert not tls._server_ok(d / "server.pem", ca)
    pki = tls.ensure_pki(d)
    assert (d / "ca.pem").read_bytes() == ca_pem and pki.ca_cert == ca
    assert _dns(_server_cert(pki)) == REDIRECT_HOSTS
    for host in REDIRECT_HOSTS:
        _handshake(tmp_path, pki.server_pem.read_bytes(), ca_pem, host)
    kept = pki.server_pem.read_bytes()
    assert tls.ensure_pki(d).server_pem.read_bytes() == kept, "a server.pem with exactly those names is kept"


def test_a_legacy_ca_keeps_working(tmp_path):
    """A CA made before (all of ubi.com and ubisoft.com) is kept as it is; its server certificate carries every
    name, wildcards included, and still chains to it; its key files become private."""
    ca, key = tls._make_ca(tls.LEGACY_PERMITTED_DOMAINS)
    d = tmp_path / "tls"
    d.mkdir()
    (d / "ca.pem").write_bytes(tls._cert_pem(ca))
    (d / "ca-key.pem").write_bytes(tls._key_pem(key))
    pki = tls.ensure_pki(d)
    assert (d / "ca.pem").read_bytes() == tls._cert_pem(ca)
    assert _dns(_server_cert(pki)) == CERT_NAMES
    for host in ("public-ubiservices.ubi.com", "dmx.upc.ubisoft.com", "other.ubi.com"):
        _handshake(tmp_path, pki.server_pem.read_bytes(), pki.ca_pem.read_bytes(), host)
    assert tls._server_ok(pki.server_pem, ca)
    if os.name == "posix":
        assert {os.stat(p).st_mode & 0o777 for p in (d / "ca-key.pem", pki.server_pem)} == {0o600}


@pytest.mark.skipif(os.name != "posix", reason="file modes")
def test_key_files_are_private(tmp_path):
    pki = tls.ensure_pki(tmp_path)
    mode = lambda p: os.stat(p).st_mode & 0o777                     # noqa: E731
    key_files = (tmp_path / "ca-key.pem", pki.server_pem)
    assert [mode(p) for p in key_files] == [0o600, 0o600]
    for p in (*key_files, pki.ca_pem):
        os.chmod(p, 0o644)
    server_before = pki.server_pem.read_bytes()
    pki = tls.ensure_pki(tmp_path)
    assert [mode(p) for p in (*key_files, pki.ca_pem)] == [0o600, 0o600, 0o644]
    assert pki.server_pem.read_bytes() == server_before, "kept, only its mode changed"
    os.chmod(pki.server_pem, 0o644)
    util.write_private(pki.server_pem, b"new")
    assert mode(pki.server_pem) == 0o600
    # the NEX signing key too
    nex = tmp_path / "nex_signing_key.pem"
    secure.load_or_create_signing_key(nex)
    assert mode(nex) == 0o600
    os.chmod(nex, 0o644)
    secure.load_or_create_signing_key(nex)
    assert mode(nex) == 0o600
