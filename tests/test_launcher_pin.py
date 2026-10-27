"""The TLS helper's certificate pin: the launcher pins the server's own leaf certificate (certs.leaf_tbs_pin,
server.leaf_der), so the in-game helper (native/hook_guard.c) accepts that certificate and no other. Fail
closed: no trusted CA or an unreachable/altered server leaves no pin, so the helper rejects everything."""
import hashlib
import socket
import ssl
import threading
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import serialization

from powder.launcher import attach, certs, server
from powder.server import tls


@dataclass
class Cfg:
    server_address: str
    server_https_port: int


def _pki(tmp_path):
    tls.ensure_pki(tmp_path)                   # ca.pem + server.pem, like a running server
    return tmp_path / "ca.pem", tmp_path / "server.pem"


def test_pin_is_sha256_of_the_tbs_certificate(tmp_path):
    _, server_pem = _pki(tmp_path)
    der = x509.load_pem_x509_certificate(server_pem.read_bytes()).public_bytes(serialization.Encoding.DER)
    pin = certs.leaf_tbs_pin(der)
    tbs = x509.load_der_x509_certificate(der).tbs_certificate_bytes
    assert pin == hashlib.sha256(tbs).digest() and len(pin) == 32


def _serve_tls(server_pem_dir):
    """A one-shot HTTPS server using the generated server.pem; returns (port, stop)."""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(server_pem_dir / "server.pem")

    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200); self.end_headers(); self.wfile.write(b"{}")

        def log_message(self, *a):
            pass

    httpd = HTTPServer(("127.0.0.1", 0), H)
    httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
    t = threading.Thread(target=httpd.serve_forever, daemon=True)
    t.start()
    return httpd.server_address[1], httpd.shutdown


def test_leaf_der_fetches_the_leaf_over_a_verified_connection(tmp_path):
    ca, server_pem = _pki(tmp_path)
    port, stop = _serve_tls(tmp_path)
    try:
        der = server.leaf_der(Cfg("127.0.0.1", port), ca, 3.0)
    finally:
        stop()
    assert der is not None
    want = x509.load_pem_x509_certificate(server_pem.read_bytes()).public_bytes(serialization.Encoding.DER)
    assert certs.leaf_tbs_pin(der) == certs.leaf_tbs_pin(want)


def test_leaf_der_fails_closed(tmp_path):
    ca, _ = _pki(tmp_path)
    # nothing listening
    assert server.leaf_der(Cfg("127.0.0.1", 1), ca, 0.5) is None
    # an unreadable / not-a-PEM CA file: None, never an exception
    bad = tmp_path / "bad.pem"
    bad.write_text("not a certificate")
    port, stop = _serve_tls(tmp_path)
    try:
        assert server.leaf_der(Cfg("127.0.0.1", port), bad, 2.0) is None
    finally:
        stop()


def test_write_and_clear_pin(tmp_path, monkeypatch):
    p = tmp_path / "hook_guard.pin"
    monkeypatch.setattr(attach, "PIN", p)
    attach.write_pin(b"\x01" * 32)
    assert p.read_bytes() == b"\x01" * 32
    attach.write_pin(b"short")                 # not 32 bytes: cleared, not written
    assert not p.exists()
    attach.write_pin(b"\x02" * 32)
    attach.clear_pin()
    assert not p.exists()
