"""The player's account on the server (registered here, used to sign in to Ubisoft Connect).

Calls go to the server's /_powder/account/* over HTTPS and are verified against the server's CA as the
Certificate step pinned it for this server (by address, so without a hostname check; session.trusted_ca()
hands only a CA file that matches the pin). Redirects are not followed and replies are read up to a limit.
Each call carries the email and current password; the launcher keeps only the email and player name
(settings.json), never the password.
"""
from __future__ import annotations

import json
import ssl
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from . import server
from .config import LauncherConfig

MAX_REPLY = 64 << 10


class AccountError(Exception):
    """The server said no (wrong password, email taken, ...), or could not be reached; the text is for the
    player."""


@dataclass(frozen=True)
class Account:
    email: str
    name: str


def _context(ca: Path) -> ssl.SSLContext:
    ctx = ssl.create_default_context(cafile=str(ca))
    ctx.check_hostname = False                    # reached by address; the CA is pinned instead
    return ctx


def _base(cfg: LauncherConfig) -> str:
    return f"https://{cfg.server_address}:{cfg.server_https_port}/_powder/account"


def call(cfg: LauncherConfig, ca: Path, what: str, body: dict | None = None, timeout: float = 10) -> dict:
    """POST (or GET without a body) /_powder/account/<what>; the reply, or AccountError."""
    if not ca.exists():
        raise AccountError("The server's certificate is not set up yet: press Play first.")
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(f"{_base(cfg)}/{what}", data=data, method="POST" if data else "GET",
                                 headers={"Content-Type": "application/json"})
    try:
        with server.open_url(req, _context(ca), timeout) as r:
            reply = json.loads(server.read_capped(r, MAX_REPLY))
        if not isinstance(reply, dict):
            raise ValueError("not a JSON object")
        return reply
    except urllib.error.HTTPError as e:
        try:
            message = json.loads(server.read_capped(e, MAX_REPLY)).get("error")
        except (ValueError, AttributeError, OSError):
            message = None
        message = str(message)[:300] if message else None
        if e.code == 404:
            raise AccountError("This server has no accounts (an older Powder): no account needed.") from e
        raise AccountError(message or f"The server refused it (HTTP {e.code}).") from e
    except (OSError, ValueError) as e:
        raise AccountError(f"The server can't be reached ({e}). Press Play to start or reach it first.") from e


def account_of(reply: dict) -> Account:
    try:
        return Account(str(reply["email"])[:254], str(reply["name"])[:64])
    except KeyError as e:
        raise AccountError("The server's reply is not what the launcher expects.") from e


def takes_over(cfg: LauncherConfig, ca: Path) -> str | None:
    """Name of the existing player a new account takes over (the first account on a server)."""
    return call(cfg, ca, "new").get("takes_over")


def register(cfg, ca, email: str, password: str, name: str) -> Account:
    return account_of(call(cfg, ca, "register", {"email": email, "password": password, "name": name}))


def sign_in(cfg, ca, email: str, password: str) -> Account:
    return account_of(call(cfg, ca, "login", {"email": email, "password": password}))


def change_email(cfg, ca, email: str, password: str, new_email: str) -> Account:
    return account_of(call(cfg, ca, "email", {"email": email, "password": password, "new_email": new_email}))


def change_password(cfg, ca, email: str, password: str, new_password: str) -> Account:
    return account_of(call(cfg, ca, "password", {"email": email, "password": password,
                                                 "new_password": new_password}))


def rename(cfg, ca, email: str, password: str, name: str) -> Account:
    return account_of(call(cfg, ca, "name", {"email": email, "password": password, "name": name}))
