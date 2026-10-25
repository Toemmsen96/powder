"""The server, from the launcher's side: start it locally (local mode) or just reach it (remote mode),
and read /_powder/status and /_powder/ca.pem. Both are fetched over the server's TLS port by address, before
the server's CA is known, so without a certificate check: the status only says what the server reports, and the
CA is checked and pinned by the Certificate step (certs.py, session.py) before anything relies on it.
Every launcher request to the server goes through open_url(): replies are read up to a limit, and redirects
are not followed (a redirect could lead a request off the pinned server, e.g. to plain HTTP)."""
from __future__ import annotations

import json
import socket
import ssl
import subprocess
import time
import urllib.request
from pathlib import Path

from .. import paths
from .config import LauncherConfig

MAX_STATUS = 1 << 20                     # bytes of /_powder/status read at most
MAX_CA = 64 << 10                        # bytes of /_powder/ca.pem read at most


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None                      # urllib then raises HTTPError with the 3xx status


def open_url(url, context: ssl.SSLContext, timeout: float):
    """urlopen() for the server: this TLS context, no redirects."""
    opener = urllib.request.build_opener(_NoRedirect, urllib.request.HTTPSHandler(context=context))
    return opener.open(url, timeout=timeout)


def read_capped(response, limit: int) -> bytes:
    """The body, or ValueError if it is larger than `limit`."""
    data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"reply larger than {limit} bytes")
    return data


class ServerProcess:
    def __init__(self, cfg: LauncherConfig):
        self.cfg = cfg
        self.proc: subprocess.Popen | None = None
        self.log_path: Path | None = None
        self.host_for: str | None = None

    def start(self, log_dir: Path, use_pack: bool = True, host_for: str | None = None) -> None:
        """host_for: this PC's address in the local network, to host for other PCs (listens on every
        network); None: as config.toml says (this PC only by default)."""
        log_dir.mkdir(parents=True, exist_ok=True)
        self.log_path = log_dir / "server-console.log"
        out = open(self.log_path, "ab")
        self.host_for = host_for
        args = ("server", "-c", str(self.cfg.server_config_path()), "run") + (() if use_pack else ("--no-gamedata",))
        if host_for:
            args += ("--bind", "0.0.0.0", "--public-host", host_for)
        self.proc = subprocess.Popen(
            paths.self_command(*args),
            stdout=out, stderr=subprocess.STDOUT,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))

    def running(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def stop(self) -> None:
        if self.running():
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def _context() -> ssl.SSLContext:
    # we connect by address, before the CA is known: no check here. These two endpoints carry no secrets; the
    # status only informs, and the CA is checked and pinned before it is used (session.check_cert)
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx


def status(cfg: LauncherConfig, timeout: float = 2.0) -> dict | None:
    try:
        with open_url(cfg.status_url, _context(), timeout) as r:
            st = json.loads(read_capped(r, MAX_STATUS))
    except (OSError, ValueError):
        return None
    return st if isinstance(st, dict) else None


def ca_pem(cfg: LauncherConfig) -> bytes | None:
    url = cfg.status_url.replace("/_powder/status", "/_powder/ca.pem")
    try:
        with open_url(url, _context(), 3) as r:
            return read_capped(r, MAX_CA)
    except (OSError, ValueError):
        return None


def leaf_der(cfg: LauncherConfig, ca_file: Path, timeout: float = 3.0) -> bytes | None:
    """The server's leaf certificate (DER), from a TLS connection verified against `ca_file` (the CA the
    player pinned for this server). None if it cannot be reached or does not verify. Used to pin the
    certificate for the TLS helper; the SNI is a redirected Ubisoft name the server's certificate covers."""
    try:
        ctx = ssl.create_default_context(cafile=str(ca_file))
        ctx.check_hostname = False                        # the server is reached by address; its cert carries the
        with socket.create_connection((cfg.server_address, cfg.server_https_port), timeout) as tcp:
            with ctx.wrap_socket(tcp, server_hostname="public-ubiservices.ubi.com") as tls:  # the CA pin is the trust
                return tls.getpeercert(binary_form=True)
    except (OSError, ssl.SSLError, ValueError):
        return None


def mine(st: dict | None, key: str, also: set[str] = frozenset()) -> list[dict]:
    """The status entries ("connect" or "game") of this PC: those from the address the server sees this
    launcher at ("you"), or from `also` (the server PC's own addresses). A server without "you" (older
    Powder): all of them."""
    entries = list((st or {}).get(key) or [])
    you = (st or {}).get("you")
    if not you:
        return entries
    ips = {you} | set(also)
    return [e for e in entries if str(e.get("peer", "")).rsplit(":", 1)[0] in ips]


def wait_up(cfg: LauncherConfig, seconds: float = 20, alive=lambda: True) -> dict | None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline and alive():
        s = status(cfg)
        if s:
            return s
        time.sleep(0.5)
    return None
