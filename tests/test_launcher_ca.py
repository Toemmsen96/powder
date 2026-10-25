"""Launcher: which server CA Windows may trust (certs.check_ca), pinning it per server (trust on first use,
host#fingerprint), the account calls only with the pinned CA, and removing exactly the CAs the launcher
installed (Restore, closing). No Windows calls: certutil and the server are faked."""
import datetime
import ipaddress
import json

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

from powder.launcher import account, certs, footprint, network, server
from powder.launcher.config import LauncherConfig
from powder.launcher.session import FAILED, JOIN, OK, Session, restore_steps
from powder.server import tls, trust
from powder.server.hostnames import REDIRECT_HOSTS
from test_launcher import FakePC

NOW = datetime.datetime.now(datetime.timezone.utc)


def make_ca(dns=("ubi.com", "ubisoft.com"), ips=("127.0.0.0/8",), path_length=0, ca=True, bc_critical=True,
            nc=True, nc_critical=True, excluded=None, other_names=(), eku=None, issuer_key=None,
            name="Powder (Steep) local CA"):
    key = ec.generate_private_key(ec.SECP256R1())
    subject = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, name)])
    b = (x509.CertificateBuilder().subject_name(subject)
         .issuer_name(subject if issuer_key is None else x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "other")]))
         .public_key(key.public_key()).serial_number(x509.random_serial_number())
         .not_valid_before(NOW - datetime.timedelta(days=1)).not_valid_after(NOW + datetime.timedelta(days=30))
         .add_extension(x509.BasicConstraints(ca=ca, path_length=path_length if ca else None), critical=bc_critical))
    if nc:
        permitted = [x509.DNSName(d) for d in dns] + [x509.IPAddress(ipaddress.ip_network(n)) for n in ips]
        permitted += list(other_names)
        b = b.add_extension(x509.NameConstraints(permitted_subtrees=permitted or None,
                                                 excluded_subtrees=excluded), critical=nc_critical)
    if eku:
        b = b.add_extension(x509.ExtendedKeyUsage(eku), critical=False)
    cert = b.sign(issuer_key or key, hashes.SHA256())
    return cert.public_bytes(serialization.Encoding.PEM), key


def test_powder_cas_are_accepted():
    py_ca, _ = tls._make_ca()                                            # the Python server's: ubi.com, ubisoft.com
    assert certs.check_ca(py_ca.public_bytes(serialization.Encoding.PEM)) == py_ca
    rust_pem, _ = make_ca(dns=REDIRECT_HOSTS)                            # the Rust server's new ones: the names
    certs.check_ca(rust_pem)
    certs.check_ca(make_ca(dns=(".ubi.com", "UBISOFT.com"), ips=("127.0.0.1/32",))[0])


ANY = ExtendedKeyUsageOID.ANY_EXTENDED_KEY_USAGE


@pytest.mark.parametrize("kw, why", [
    (dict(nc=False), "NameConstraints"),
    (dict(nc_critical=False), "NameConstraints"),
    (dict(dns=("com",)), "outside"),
    (dict(dns=("ubi.com", "google.com")), "outside"),
    (dict(dns=("ubi.com.evil.example",)), "outside"),
    (dict(dns=("evilubi.com",)), "outside"),
    (dict(dns=("",)), "outside"),
    (dict(dns=()), "unconstrained"),
    (dict(ips=()), "unconstrained"),
    (dict(ips=("0.0.0.0/0",)), "127.0.0.0/8"),
    (dict(ips=("10.0.0.0/8",)), "127.0.0.0/8"),
    (dict(ips=("::/0",)), "127.0.0.0/8"),
    (dict(excluded=[x509.DNSName("x.ubi.com")]), "excluded"),
    (dict(other_names=[x509.RFC822Name("ubi.com")]), "other kinds"),
    (dict(path_length=None), "path length"),
    (dict(path_length=1), "path length"),
    (dict(ca=False), "path length"),
    (dict(bc_critical=False), "path length"),
    (dict(eku=[ExtendedKeyUsageOID.CODE_SIGNING]), "more than TLS"),
    (dict(eku=[ANY]), "more than TLS"),
])
def test_cas_that_could_vouch_for_more_are_refused(kw, why):
    pem, _ = make_ca(**kw)
    with pytest.raises(certs.CaError, match=why):
        certs.check_ca(pem)


def test_only_one_self_signed_certificate():
    good, key = make_ca()
    other, _ = make_ca()
    key_pem = key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
                                serialization.NoEncryption())
    for bad in (good + other, good + key_pem, b"", b"garbage", good.replace(b"A", b"B", 5), b"\xff" + good,
                good + b"x" * certs.MAX_PEM):
        with pytest.raises(certs.CaError):
            certs.check_ca(bad)
    signed_by_other, _ = make_ca(issuer_key=ec.generate_private_key(ec.SECP256R1()))
    with pytest.raises(certs.CaError, match="self-signed"):
        certs.check_ca(signed_by_other)


def test_saved_ca_is_the_certificate_alone(tmp_path):
    pem, _ = make_ca()
    path = certs.save_ca(certs.check_ca(b"some text before\n" + pem + b"\n"), tmp_path / "ca.pem")
    assert path.read_bytes() == pem


# ---- the Certificate step: pinned per server -------------------------------------------------------------
class Asking(Session):
    def __init__(self, cfg, data_dir, answers=()):
        super().__init__(cfg, data_dir)
        self.answers, self.asked = list(answers), []

    def ask(self, key, text, actions, details=""):
        self.asked.append((key, text, actions))
        return self.answers.pop(0)


@pytest.fixture
def windows(monkeypatch):
    """The server's /_powder/ca.pem and Windows' trusted roots, faked."""
    w = type("W", (), {})()
    w.pem, w.trusted, w.installs = make_ca()[0], set(), []

    def install(path):
        cert = x509.load_pem_x509_certificate(path.read_bytes())
        w.installs.append(certs.thumbprint(cert))
        w.trusted.add(certs.thumbprint(cert))
    monkeypatch.setattr(server, "ca_pem", lambda cfg: w.pem)
    monkeypatch.setattr(certs, "install", install)
    monkeypatch.setattr(certs, "is_trusted", lambda path: certs.thumbprint(certs.load(path)) in w.trusted)
    return w


def fingerprint(pem: bytes) -> str:
    return certs.sha256(x509.load_pem_x509_certificate(pem))


def cfg_for(tmp_path, **kw) -> LauncherConfig:
    return LauncherConfig(server_config=str(tmp_path / "srv" / "config.toml"), **kw)


def test_first_use_shows_the_fingerprint_and_asks(tmp_path, windows):
    s = Asking(cfg_for(tmp_path), tmp_path, answers=["Cancel"])
    assert not s.check_cert() and s.step("cert").state == FAILED
    (key, text, actions), = s.asked
    assert actions == ["Trust this server", "Cancel"] and certs.pretty(fingerprint(windows.pem)) in text
    assert windows.installs == [] and s.pinned_ca() is None and s.trusted_ca() is None

    s = Asking(cfg_for(tmp_path), tmp_path, answers=["Trust this server", "Install certificate"])
    assert s.check_cert() and s.step("cert").state == OK
    assert s.pinned_ca() == fingerprint(windows.pem) and s.trusted_ca() == s.ca_path
    assert s.launcher_cas() == windows.installs and len(windows.installs) == 1
    assert Asking(cfg_for(tmp_path), tmp_path).check_cert()             # the same CA again: no question


def test_another_ca_stops_unless_the_player_confirms(tmp_path, windows):
    s = Asking(cfg_for(tmp_path), tmp_path, answers=["Trust this server", "Install certificate"])
    assert s.check_cert()
    first, saved = s.pinned_ca(), s.ca_path.read_bytes()
    windows.pem = make_ca()[0]                                            # someone else answers now
    s = Asking(cfg_for(tmp_path), tmp_path, answers=["Stop"])
    assert not s.check_cert() and "WARNING" in s.asked[0][1] and s.asked[0][2][0] == "Stop"
    assert s.pinned_ca() == first and s.ca_path.read_bytes() == saved and len(windows.installs) == 1
    assert s.trusted_ca() == s.ca_path                                    # the account calls keep the old CA

    s = Asking(cfg_for(tmp_path), tmp_path, answers=["The host made a new CA: trust it", "Install certificate"])
    assert s.check_cert() and s.pinned_ca() == fingerprint(windows.pem) and len(windows.installs) == 2


def test_join_address_fingerprint_decides(tmp_path, windows):
    fp = fingerprint(windows.pem)
    s = Asking(cfg_for(tmp_path), tmp_path, answers=["Install certificate"])
    s.set_mode(JOIN, join_address=f"192.0.2.10#{certs.pretty(fp[:24]).replace(' ', ':')}")
    assert s.cfg.server_address == "192.0.2.10" and s.check_cert()
    assert [k for k, _, _ in s.asked] == ["cert"] and s.asked[0][2] == ["Install certificate"]
    assert s.pinned_ca() == fp

    other = make_ca()[0]
    s = Asking(cfg_for(tmp_path), tmp_path / "other")
    s.set_mode(JOIN, join_address=f"192.0.2.10#{fingerprint(other)[:16]}")
    assert not s.check_cert() and s.asked == [] and "intercepting" in s.step("cert").text
    assert len(windows.installs) == 1 and s.pinned_ca() is None


def test_bad_join_fingerprint_is_refused(tmp_path):
    s = Asking(cfg_for(tmp_path), tmp_path)
    s.set_mode(JOIN, join_address="192.0.2.10#abc")
    assert not s.check_server() and "fingerprint" in s.step("server").text


def test_the_local_servers_own_ca_needs_no_question(tmp_path, windows, monkeypatch):
    monkeypatch.setattr(network, "lan_addresses", lambda: ["192.0.2.10"])
    ca_pem = tmp_path / "srv" / "data" / "tls" / "ca.pem"
    ca_pem.parent.mkdir(parents=True)
    ca_pem.write_bytes(windows.pem)
    s = Asking(cfg_for(tmp_path), tmp_path, answers=["Install certificate"])
    assert s.check_cert() and [a for _, _, a in s.asked] == [["Install certificate"]]
    s.set_mode("host", host_address="192.0.2.10")                       # what the others enter under Join
    assert s.join_text() == f"192.0.2.10#{fingerprint(windows.pem)[:24]}"


def test_a_refused_ca_never_reaches_windows(tmp_path, windows):
    windows.pem = make_ca(nc=False)[0]
    s = Asking(cfg_for(tmp_path), tmp_path)
    assert not s.check_cert() and s.asked == [] and windows.installs == []
    assert "refused" in s.step("cert").text and not s.ca_path.exists()


def test_account_calls_only_with_the_pinned_ca(tmp_path, windows, monkeypatch):
    s = Asking(cfg_for(tmp_path), tmp_path)
    with pytest.raises(account.AccountError, match="press Play"):
        s.account_action("sign_in", email="a@b.c", password="x")
    s.ca_path.write_bytes(windows.pem)                                    # a CA file without a pin (older launcher)
    with pytest.raises(account.AccountError, match="press Play"):
        s.account_action("sign_in", email="a@b.c", password="x")
    s._pin_ca(fingerprint(make_ca()[0]))                                  # pinned: another one
    assert s.trusted_ca() is None
    s._pin_ca(fingerprint(windows.pem))
    seen = []
    monkeypatch.setattr(account, "sign_in", lambda cfg, ca, **kw: seen.append(ca) or account.Account("a@b.c", "A"))
    s.account_action("sign_in", email="a@b.c", password="x")
    assert seen == [s.ca_path]


# ---- removing exactly what the launcher installed ---------------------------------------------------------
def test_restore_removes_recorded_cas_whatever_their_name(tmp_path, monkeypatch):
    pc = FakePC(hosts=False, cas=1, connect=False)
    pc.install(monkeypatch)
    odd = "AB" * 20
    monkeypatch.setattr(trust, "trusted_thumbprints", lambda: {t for t, _ in pc.cas} | ({odd} if odd_there else set()))
    odd_there = True
    (tmp_path / "settings.json").write_text(json.dumps({"trusted_cas": [odd, "not a thumbprint"]}))
    assert len(footprint.scan(LauncherConfig(), tmp_path).cas) == 2

    def remove(thumb):
        nonlocal odd_there
        pc.calls.append(("cert", thumb))
        pc.cas = [c for c in pc.cas if c[0] != thumb]
        odd_there = odd_there and thumb != odd
    monkeypatch.setattr(trust, "remove_thumbprint", remove)
    s = Asking(LauncherConfig(), tmp_path, answers=["Remove certificate", "Done"])
    s.steps = restore_steps()
    s._run(s.restore_flow())
    assert pc.calls == [("cert", "T0"), ("cert", odd)] and s.step("cert").state == OK
    assert s.launcher_cas() == [] and footprint.scan(LauncherConfig(), tmp_path).clean


def test_closing_removes_the_ca_only_when_asked_for(tmp_path, monkeypatch):
    removed = []
    monkeypatch.setattr(trust, "remove_thumbprint", removed.append)
    s = Session(LauncherConfig(), tmp_path)
    s.installed_ca = "CD" * 20
    assert not s.ca_removal_on_exit
    s.stop()
    assert removed == []
    s = Session(LauncherConfig(remove_ca_on_exit=True), tmp_path)
    s.installed_ca = "CD" * 20
    s._remember_installed(s.installed_ca)
    assert s.ca_removal_on_exit
    s.stop(remove_ca=False)
    assert removed == []
    s.stop()
    assert removed == ["CD" * 20] and s.launcher_cas() == [] and not s.ca_removal_on_exit


def test_server_replies_are_capped_and_redirects_not_followed(monkeypatch):
    import io
    import urllib.request
    assert server._NoRedirect().redirect_request(urllib.request.Request("https://192.0.2.10/"), None, 302, "Found",
                                                 {}, "http://evil.example/") is None
    assert server.read_capped(io.BytesIO(b"x" * 10), 10) == b"x" * 10
    with pytest.raises(ValueError):
        server.read_capped(io.BytesIO(b"x" * 11), 10)
    replies = {"status": b"{" + b" " * server.MAX_STATUS + b"}", "ca.pem": b"x" * (server.MAX_CA + 1)}
    monkeypatch.setattr(server, "open_url", lambda url, ctx, timeout: io.BytesIO(replies[url.rsplit("/", 1)[1]]))
    assert server.status(LauncherConfig()) is None and server.ca_pem(LauncherConfig()) is None
    replies.update({"status": b'["not", "an object"]', "ca.pem": b"pem"})
    assert server.status(LauncherConfig()) is None and server.ca_pem(LauncherConfig()) == b"pem"
