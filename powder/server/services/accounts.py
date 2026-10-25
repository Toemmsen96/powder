"""Accounts, login and session lookup: game and Ubisoft Connect requests -> Player -> PlayerSession.

An account (email + password, registered in the launcher) owns one player. Ubisoft Connect signs in
with the email and password (`Basic`), later with the remember-me ticket we gave it (`rm_v1`, kept in
the database so it outlives a server restart). The game logs in with a ticket Ubisoft Connect made
(`uplaypc_v1`); it is the player Connect signed in as on the same PC.

Before the first account exists (and unless [server] public is set), a server works as it did before
accounts: every credential is accepted and the AccountResolver ([players.bind], else the default player)
decides who it is. Only a registration from the server's own PC (loopback) takes over the default player.

Lenient mode (default, [server] strict_tickets = false): a ticket we do not know (e.g. the server
was restarted while the game kept running) is not rejected; the request is attributed to the latest
session from the same address (or, without accounts, via the AccountResolver) and a warning is
logged. Strict mode refuses it; a public server ([server] public) is always strict.

The game's own login (`uplaypc_v1`, a ticket Ubisoft Connect made that we cannot read) is the player whose
Connect signed in last from the same address. Nothing we serve ties that ticket to one Connect session, so
players behind one NAT address who start their games at the same time can be taken for each other there.

Passwords are hashed with scrypt and never logged or stored in clear. A hash takes tens of milliseconds and
16 MiB: the HTTPS routes use the *_async methods, which hash on a few worker threads (MAX_CONCURRENT_HASHES)
instead of blocking the event loop; more sign-ins wait their turn.
"""
from __future__ import annotations

import asyncio
import base64
import binascii
import hashlib
import hmac
import ipaddress
import logging
import re
import secrets
import uuid
from concurrent.futures import ThreadPoolExecutor
from typing import Callable

from ..config import Config
from ..constants import APP_ID
from ..core.identity import AccountResolver, DefaultResolver, LoginRequest, Player
from ..core.sessions import PlayerSession, SessionRegistry
from ..db.repos import AccountRow, Repos

log = logging.getLogger("powder.accounts")

MIN_PASSWORD = 8
NAME_RE = re.compile(r"^[A-Za-z0-9._-]{3,15}$")         # like Ubisoft names: 3-15 letters, digits, . _ -
EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
SCRYPT_N, SCRYPT_R, SCRYPT_P = 2 ** 14, 8, 1
MAX_CONCURRENT_HASHES = 4                               # scrypt runs at once for the routes (16 MiB each)
_HASHING = ThreadPoolExecutor(max_workers=MAX_CONCURRENT_HASHES, thread_name_prefix="powder-hash")


class AuthError(Exception):
    """Login refused (wrong credentials, unknown remember-me ticket, ...)."""


class AccountError(ValueError):
    """An account request that cannot be done (email taken, password too short, ...); the message is
    meant for the player."""


def hash_password(password: str) -> str:
    salt = secrets.token_bytes(16)
    key = hashlib.scrypt(password.encode(), salt=salt, n=SCRYPT_N, r=SCRYPT_R, p=SCRYPT_P, dklen=32)
    return f"scrypt${SCRYPT_N}${SCRYPT_R}${SCRYPT_P}${salt.hex()}${key.hex()}"


def check_password(password: str, stored: str) -> bool:
    try:
        algo, n, r, p, salt, key = stored.split("$")
        if algo != "scrypt":
            return False
        got = hashlib.scrypt(password.encode(), salt=bytes.fromhex(salt), n=int(n), r=int(r), p=int(p),
                             dklen=len(key) // 2)
    except (ValueError, TypeError):
        return False
    return hmac.compare_digest(got.hex(), key)


_DUMMY_HASH = hash_password(secrets.token_urlsafe(16))     # checked for unknown emails (see authenticate)


async def off_loop(fn, *args):
    """fn(*args) on one of the hashing threads (hashlib.scrypt releases the GIL), so the event loop goes on."""
    return await asyncio.get_running_loop().run_in_executor(_HASHING, fn, *args)


def may_take_over(client_ip: str) -> bool:
    """Whether a registration from this address may take over the default player: only from the server's own
    PC (127.0.0.0/8, ::1, IPv4-mapped loopback)."""
    try:
        ip = ipaddress.ip_address(client_ip)
    except ValueError:
        return False
    return ip.is_loopback or bool(ip.version == 6 and ip.ipv4_mapped and ip.ipv4_mapped.is_loopback)


def basic_credentials(value: str) -> tuple[str, str] | None:
    """`Basic` value -> (email, password), or None if it holds none (the game sends empty ones)."""
    try:
        raw = base64.b64decode(value, validate=True).decode("utf-8")
    except (binascii.Error, UnicodeDecodeError, ValueError):
        return None
    email, sep, password = raw.partition(":")
    return (email.strip(), password) if sep and email.strip() else None


class AccountService:
    def __init__(self, config: Config, repos: Repos, sessions: SessionRegistry,
                 seed: Callable[..., None], default_profile_id: Callable[[], str]):
        self.config, self.repos, self.sessions = config, repos, sessions
        self.resolver: AccountResolver = DefaultResolver(config, default_profile_id)
        self.default_profile_id = default_profile_id
        self._seed = seed

    # ---- players ------------------------------------------------------------------------------
    def player(self, profile_id: str) -> Player:
        """The player record, created (and seeded) on first use."""
        row = self.repos.players.get(profile_id)
        if row is None:
            self._seed(profile_id)
            row = self.repos.players.get(profile_id)
        return Player(row.profile_id, row.user_id, row.name)

    @property
    def required(self) -> bool:
        """Accounts are required once the first one exists, and always on a public server."""
        return self.config.server.public or self.repos.accounts.count() > 0

    @property
    def strict_tickets(self) -> bool:
        """Unknown tickets are refused: [server] strict_tickets, and always on a public server (the lenient
        fallback would make players behind one NAT address each other)."""
        return self.config.server.strict_tickets or self.config.server.public

    # ---- accounts -----------------------------------------------------------------------------
    def register(self, email: str, password: str, name: str) -> AccountRow:
        """A player registering on the server's own PC (see register_from)."""
        return self.register_from(email, password, name, "127.0.0.1")

    def register_from(self, email: str, password: str, name: str, client_ip: str) -> AccountRow:
        """A player registering from `client_ip`: refused while [players] registration = "closed". Only from
        the server's own PC (loopback) may the first account take over the default player; from elsewhere it
        gets a new player, so a stranger can't claim the host's (imported) progress on a fresh server."""
        email = self._check_registration(email, password)
        return self._new_account(email, hash_password(password), name, may_take_over(client_ip))

    async def register_async(self, email: str, password: str, name: str, client_ip: str) -> AccountRow:
        """register_from, hashing off the event loop."""
        email = self._check_registration(email, password)
        return self._new_account(email, await off_loop(hash_password, password), name, may_take_over(client_ip))

    def create_account(self, email: str, password: str, name: str) -> AccountRow:
        """The host's `admin account-create`: also while registrations are closed; the first account takes
        over the default player, like one registered on the server's PC."""
        email = email.strip()
        self._check_email(email)
        self._check_password(password)
        return self._new_account(email, hash_password(password), name, True)

    def _check_registration(self, email: str, password: str) -> str:
        if self.config.players.registration == "closed":
            raise AccountError("This server does not take registrations; ask its host for an account.")
        email = email.strip()
        self._check_email(email)
        self._check_password(password)
        return email

    def _new_account(self, email: str, password_hash: str, name: str, take_over: bool) -> AccountRow:
        """A new account with its own player. With `take_over`, the first account on a server takes over the
        existing default player (and its progress) if no account owns it yet."""
        name = name.strip()
        with self.repos.db.transaction():
            self._check_email(email)                    # again: another registration may have come meanwhile
            profile_id = self._claimable_player() if take_over else None
            if profile_id is None:
                self._check_name(name)
                profile_id = str(uuid.uuid4())
                self._seed(profile_id, name=name)
            elif name and name != self.repos.players.get(profile_id).name:
                self._check_name(name, profile_id)
                self.repos.players.rename(profile_id, name)
            account = self.repos.accounts.create(str(uuid.uuid4()), email, password_hash, profile_id)
        log.info("account %s registered for player %s (%s)", account.account_id,
                 self.repos.players.get(profile_id).name, profile_id)
        return account

    def claimable_player_name(self) -> str | None:
        """Name of the player a new account would take over (see register), else None."""
        pid = self._claimable_player()
        return self.repos.players.get(pid).name if pid else None

    def claimable_player_name_for(self, client_ip: str) -> str | None:
        """The same for a registration from `client_ip` (see register_from)."""
        return self.claimable_player_name() if may_take_over(client_ip) else None

    def _claimable_player(self) -> str | None:
        if self.repos.accounts.count():
            return None
        pid = self.default_profile_id()
        return pid if self.repos.players.get(pid) else None

    def authenticate(self, email: str, password: str) -> AccountRow:
        account = self.repos.accounts.by_email(email or "")
        # an unknown email costs as much as a wrong password, so emails can't be probed by timing
        if not check_password(password or "", account.password_hash if account else _DUMMY_HASH) or not account:
            raise AuthError("wrong email or password")
        return account

    async def authenticate_async(self, email: str, password: str) -> AccountRow:
        """authenticate, hashing off the event loop."""
        account = self.repos.accounts.by_email(email or "")
        ok = await off_loop(check_password, password or "", account.password_hash if account else _DUMMY_HASH)
        account = self.repos.accounts.get(account.account_id) if account else None    # as it is now
        if not ok or not account:
            raise AuthError("wrong email or password")
        return account

    def change_email(self, account: AccountRow, new_email: str) -> AccountRow:
        new_email = new_email.strip()
        self._check_email(new_email, account.account_id)
        self.repos.accounts.update(account.account_id, email=new_email)
        log.info("account %s: email changed", account.account_id)
        return self.repos.accounts.get(account.account_id)

    def change_password(self, account: AccountRow, new_password: str) -> AccountRow:
        """Ubisoft Connect's remember-me tickets stop working, so its next sign-in needs the new password; a
        game that is running now stays connected."""
        self._check_password(new_password)
        return self._set_password(account, hash_password(new_password))

    async def change_password_async(self, account: AccountRow, new_password: str) -> AccountRow:
        """change_password, hashing off the event loop."""
        self._check_password(new_password)
        return self._set_password(account, await off_loop(hash_password, new_password))

    def _set_password(self, account: AccountRow, password_hash: str) -> AccountRow:
        self.repos.accounts.update(account.account_id, password_hash=password_hash)
        revoked = self.repos.remember_tickets.revoke_all(account.account_id)
        log.info("account %s: password changed, %d remember-me ticket(s) revoked", account.account_id, revoked)
        return self.repos.accounts.get(account.account_id)

    def rename(self, account: AccountRow, name: str) -> None:
        name = name.strip()
        self._check_name(name, account.profile_id)
        self.repos.players.rename(account.profile_id, name)
        log.info("account %s: player renamed to %s", account.account_id, name)

    def link(self, account: AccountRow, profile_id: str) -> None:
        """Admin: let an account own another (existing) player."""
        profile_id = profile_id.lower()
        if self.repos.players.get(profile_id) is None:
            raise AccountError(f"no player {profile_id}")
        other = self.repos.accounts.by_profile(profile_id)
        if other and other.account_id != account.account_id:
            raise AccountError(f"player {profile_id} belongs to account {other.email}")
        self.repos.accounts.update(account.account_id, profile_id=profile_id)
        self.sessions.drop_player(account.profile_id)

    def delete(self, account: AccountRow) -> None:
        """Admin: remove an account; its player and progress stay (link them to another account)."""
        self.repos.accounts.delete(account.account_id)
        self.sessions.drop_player(account.profile_id)

    def info(self, account: AccountRow) -> dict:
        return {"account_id": account.account_id, "email": account.email, "profile_id": account.profile_id,
                "name": self.player(account.profile_id).name}

    def _check_email(self, email: str, account_id: str | None = None) -> None:
        if not EMAIL_RE.match(email):
            raise AccountError("That doesn't look like an email address.")
        other = self.repos.accounts.by_email(email)
        if other and other.account_id != account_id:
            raise AccountError("An account with this email already exists.")

    @staticmethod
    def _check_password(password: str) -> None:
        if len(password or "") < MIN_PASSWORD:
            raise AccountError(f"The password needs at least {MIN_PASSWORD} characters.")

    def _check_name(self, name: str, profile_id: str | None = None) -> None:
        if not NAME_RE.match(name):
            raise AccountError("The name needs 3 to 15 letters, digits, '.', '_' or '-'.")
        other = self.repos.players.by_name(name)
        if other and other.profile_id != (profile_id or "").lower():
            raise AccountError("Another player already has this name.")

    # ---- login --------------------------------------------------------------------------------
    def login(self, req: LoginRequest) -> PlayerSession:
        """POST /v3/profiles/sessions -> a new session of ours, or AuthError."""
        profile_id, how = self._identify(req)
        return self._issue(req, profile_id, how)

    async def login_async(self, req: LoginRequest) -> PlayerSession:
        """login, checking an email and password (`Basic`) off the event loop."""
        creds = basic_credentials(req.auth_value) if req.auth_scheme.lower() == "basic" else None
        if creds and self.required:
            return self._issue(req, (await self.authenticate_async(*creds)).profile_id, "email and password")
        return self.login(req)

    def _issue(self, req: LoginRequest, profile_id: str, how: str) -> PlayerSession:
        app_id = {k.lower(): v for k, v in req.headers.items()}.get("ubi-appid", "").lower()
        connect = app_id if app_id and app_id != APP_ID else ""        # Ubisoft Connect and its parts
        session = self.sessions.issue(self.player(profile_id), req.client_ip, app_id=connect)
        log.info("login %s (%s) from %s%s: %s", session.player.name, profile_id, req.client_ip,
                 " [Ubisoft Connect]" if connect else "", how)
        return session

    def remember(self, session: PlayerSession, ticket: str) -> None:
        """Keep a remember-me ticket handed to Ubisoft Connect, for its next sign-in (rm_v1)."""
        account = self.repos.accounts.by_profile(session.profile_id)
        if account:
            self.repos.remember_tickets.add(ticket, account.account_id)

    def _identify(self, req: LoginRequest) -> tuple[str, str]:
        """-> (profile id, which rule decided). Credentials are never logged."""
        scheme = req.auth_scheme.lower()
        required = self.required
        if scheme == "basic":
            creds = basic_credentials(req.auth_value)
            if creds and required:
                return self.authenticate(*creds).profile_id, "email and password"
        elif scheme == "rm_v1" and req.auth_value:
            account_id = self.repos.remember_tickets.account(req.auth_value)
            account = self.repos.accounts.get(account_id) if account_id else None
            if account:
                return account.profile_id, "remember-me ticket"
            if required:
                raise AuthError("unknown or revoked remember-me ticket")
        if req.auth_value:
            s = self.sessions.by_token(req.auth_value)
            if s:
                return s.profile_id, "ticket of ours"
        if required:
            s = self.sessions.latest_from(req.client_ip, connect_only=True)
            if s:
                return s.profile_id, "Ubisoft Connect signed in from the same address"
            raise AuthError("no account signed in from this PC (sign in to Ubisoft Connect first)")
        profile_id = self.resolver.resolve(req)
        if not profile_id:
            raise AuthError("login refused by account resolver")
        return profile_id, "no accounts yet: address/default player"

    def session_for_ticket(self, ticket: str, client_ip: str) -> PlayerSession | None:
        """HTTPS `Ubi_v1 t=` ticket or NEX LoginWithToken token -> session (lenient fallback)."""
        s = self.sessions.by_token(ticket) if ticket else None
        if s or self.strict_tickets:
            return s
        if self.required:
            s = self.sessions.latest_from(client_ip)
            if s and ticket:
                log.warning("unknown ticket from %s -> attributed to %s (lenient mode)", client_ip, s.player.name)
            return s
        profile_id = self.resolver.resolve(LoginRequest(client_ip=client_ip))
        if not profile_id:
            return None
        s = self.sessions.for_player(profile_id)
        if s is None:
            s = self.sessions.issue(self.player(profile_id), client_ip)
        if ticket:
            log.warning("unknown ticket from %s -> attributed to %s (lenient mode)", client_ip, profile_id)
        return s
