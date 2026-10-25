"""Privacy check for a game-data pack: nothing that identifies a person may end up in it.

Markers are collected from the *personal* parts of the source recordings (everything the builder did not
allow-list): the values of identifying JSON keys (profile/user ids, names, emails, tickets, ip), every
GUID in friend lists, profile lookups and leaderboards, and Connect's friends. Then every file of the pack
(including base64 bodies, decoded, and what is inside zip / gzip bodies) is searched for them. Any hit fails
the check. Markers are at least 6 characters (shorter ids would match by chance), except names: a player name
can be 3 characters (accounts.NAME_RE), so names of 3-5 are searched as whole words (case-sensitive).

Besides, a product entry's field 21 (40 hex digits) must be Steep's known value (builtin/product_table.py):
another value could be per account. And self_check() looks for account data without the recordings (a pack
being zipped for someone else): values of identifying keys, e-mail addresses, field 21.
"""
from __future__ import annotations

import base64
import gzip
import io
import json
import re
import zipfile
from pathlib import Path

from ..transport.demux import pb

GUID = re.compile(rb"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
IDENTIFYING_KEYS = {"profileid", "userid", "idonplatform", "nameonplatform", "username", "email",
                    "emailaddress", "ticket", "sessionkey", "sessionid", "remembermeticket", "clientip",
                    "firstname", "lastname", "dateofbirth", "friendid", "pid"}
# replies that are lists of people: every GUID in them is a marker. (Rider cards and inventories are not
# here: they also hold game item ids that legitimately appear in the store catalogue; their owner is
# caught through the profileId key.)
PEOPLE_PATHS = re.compile(r"/friends|/v3/profiles$|/onlineStatuses|/blocks|harboursocial")
NAME_KEYS = {"nameonplatform", "username", "firstname", "lastname"}
MIN_MARKER, MIN_NAME = 6, 3
PRODUCT_HASH = 21                                   # a product entry's 40-hex field
HEX40 = re.compile(rb"[0-9A-Fa-f]{40}")
EMAIL = re.compile(rb"[\w.+-]{1,64}@[\w-]{1,63}(\.[\w-]{1,63})+")
MAX_INNER = 64 << 20                                # what is unpacked from one zip / gzip body to search it


class Name(str):
    """A marker that is a person's name: searched down to MIN_NAME characters, as a whole word."""


def _walk(value, out: set[str]) -> None:
    if isinstance(value, dict):
        for k, v in value.items():
            if k.lower() in IDENTIFYING_KEYS and isinstance(v, (str, int)) and len(str(v)) >= MIN_MARKER:
                out.add(str(v))
            elif k.lower() in NAME_KEYS and isinstance(v, str) and len(v.strip()) >= MIN_NAME:
                out.add(Name(v.strip()))
            _walk(v, out)
    elif isinstance(value, list):
        for v in value:
            _walk(v, out)


def markers_from_http(entries: list[dict], kept: set[int]) -> set[str]:
    """Markers from recorded HTTPS replies that were NOT copied into the pack (`kept` = their indexes)."""
    out: set[str] = set()
    for i, e in enumerate(entries):
        if i in kept:
            continue
        body = base64.b64decode(e.get("body_b64", ""))
        try:
            _walk(json.loads(body), out)
        except (ValueError, UnicodeDecodeError):
            pass
        if PEOPLE_PATHS.search(e["path"]):
            out.update(g.decode().lower() for g in GUID.findall(body))
    return out


def markers_from_rmc(rmc: dict, kept: set[str]) -> set[str]:
    """Every GUID in NEX replies that were not kept (leaderboards, profiles, matchmaking)."""
    out: set[str] = set()
    for method, rows in rmc.items():
        if method in kept:
            continue
        for row in rows:
            out.update(g.decode().lower() for g in GUID.findall(bytes.fromhex(row.get("body", ""))))
    return out


def _friend_names(raw: bytes) -> set[str]:
    """Names in a friends_service initialize reply: {1: {1 id, 22: {2: {2: {1 profile id, 2 name}}...}}}."""
    names = set()
    try:
        body = pb.decode(pb.first(pb.decode(pb.first(pb.decode(raw), 1)), 22) or b"")
    except (pb.DecodeError, TypeError):
        return names
    for n, entry, _ in body:
        if n == 2 and isinstance(entry, bytes):
            inner = pb.try_decode(pb.first(pb.try_decode(entry) or [], 2) or b"") or []
            name = pb.first(inner, 2)
            if isinstance(name, bytes) and len(name.strip()) >= MIN_NAME:
                names.add(Name(name.decode("utf-8", "replace").strip()))
    return names


def markers_from_demux(demux: dict) -> set[str]:
    """Friends (ids, names) and the player's id from Connect's demux replies."""
    out: set[str] = set()
    for svc, kinds in demux.get("services", {}).items():
        if svc == "ownership_service":
            continue                                   # product data, handled by the allow-list
        for kind, replies in kinds.items():
            for b64 in replies:
                raw = base64.b64decode(b64)
                out.update(g.decode().lower() for g in GUID.findall(raw))
                if svc == "friends_service" and kind == "22":
                    out.update(_friend_names(raw))
    return out


def _inner(blob: bytes):
    """What a zip or gzip body holds (capped), to be searched too."""
    if blob[:4] == b"PK\x03\x04":
        try:
            with zipfile.ZipFile(io.BytesIO(blob)) as z:
                left = MAX_INNER
                for info in z.infolist():
                    if info.is_dir() or info.file_size > left:
                        continue
                    left -= info.file_size
                    yield info.filename, z.read(info)
        except (zipfile.BadZipFile, zipfile.LargeZipFile, OSError, EOFError, NotImplementedError):
            return
    elif blob[:2] == b"\x1f\x8b":
        try:
            with gzip.GzipFile(fileobj=io.BytesIO(blob)) as g:
                yield "gzip", g.read(MAX_INNER)
        except (OSError, EOFError):
            return


def _pack_blobs(root: Path):
    for path in sorted(root.rglob("*.json")):
        raw = path.read_bytes()
        name = path.relative_to(root).as_posix()
        yield name, raw
        try:
            data = json.loads(raw)
        except ValueError:
            continue
        for b in _b64_strings(data):
            yield f"{name} (decoded)", b
            for inner, content in _inner(b):
                yield f"{name} (decoded, {inner[:40]})", content


def _b64_strings(value):
    if isinstance(value, dict):
        for k, v in value.items():
            if isinstance(v, str) and (k.endswith("b64") or k in ("patch_info",)):
                yield base64.b64decode(v)
            else:
                yield from _b64_strings(v)
    elif isinstance(value, list):
        for v in value:
            if isinstance(v, str) and len(v) > 40 and re.fullmatch(r"[A-Za-z0-9+/=]+", v):
                try:
                    yield base64.b64decode(v)
                except ValueError:
                    pass
            else:
                yield from _b64_strings(v)


def known_product_hash(product_id) -> bytes | None:
    from ..builtin.product_table import PRODUCTS
    for fields in PRODUCTS:
        if fields[0][1] == product_id:
            value = dict(fields).get(PRODUCT_HASH)
            return value.encode() if isinstance(value, str) else value or None
    return None


def product_hash_findings(root: Path) -> list[str]:
    """Products in connect/demux.json whose field 21 (40 hex digits) is not Steep's known one: maybe per account."""
    path = root / "connect" / "demux.json"
    try:
        products = json.loads(path.read_text(encoding="utf-8")).get("products", []) if path.exists() else []
    except (ValueError, AttributeError):
        return []
    findings = []
    for b64 in products if isinstance(products, list) else []:
        fields = pb.try_decode(base64.b64decode(b64)) if isinstance(b64, str) else None
        if not fields:
            continue
        pid, value = pb.first(fields, 1), pb.first(fields, PRODUCT_HASH)
        if isinstance(value, bytes) and HEX40.fullmatch(value) and value.upper() != (known_product_hash(pid) or b""):
            findings.append(f"connect/demux.json: product {pid} field {PRODUCT_HASH} {value[:8].decode()}... "
                            "(not Steep's known value: maybe per account)")
    return findings


def check(root: Path, markers: set[str], allowed: set[str] = frozenset()) -> list[str]:
    """-> findings 'file: marker' (empty = clean). `allowed` = markers known to be game-wide (e.g. the
    game's own app/space ids, which also appear inside personal replies)."""
    markers = {m for m in markers if m not in allowed}
    lowered = [(m, m.encode().lower()) for m in markers if len(m) >= MIN_MARKER]
    names = [(m, re.compile(rb"(?<![\w])" + re.escape(m.encode()) + rb"(?![\w])")) for m in markers
             if isinstance(m, Name) and MIN_NAME <= len(m) < MIN_MARKER]
    findings = product_hash_findings(root)
    for name, blob in _pack_blobs(root):
        low = blob.lower()
        for m, n in lowered:
            if n in low:
                findings.append(f"{name}: {m[:12]}...")
        for m, rx in names:
            if rx.search(blob):
                findings.append(f"{name}: {m[:12]}...")
    return sorted(set(findings))


def _identifying_values(value, out: list) -> None:
    if isinstance(value, dict):
        for k, v in value.items():
            if k.lower() in IDENTIFYING_KEYS | NAME_KEYS and isinstance(v, (str, int)) and not isinstance(v, bool) \
                    and str(v).strip() and not re.fullmatch(r"\{.*\}|0", str(v)):
                out.append(f"{k}={str(v)[:12]}")
            _identifying_values(v, out)
    elif isinstance(value, list):
        for v in value:
            _identifying_values(v, out)


def self_check(root: Path) -> list[str]:
    """The checks that need no recordings (a pack zipped for someone else): values of identifying keys (not
    the game's own ids), e-mail addresses, product field 21. -> findings."""
    from ..constants import APP_ID, CONNECT_APP_ID, CONNECT_SPACE_ID, SPACE_ID
    game_ids = {APP_ID, SPACE_ID, CONNECT_APP_ID, CONNECT_SPACE_ID}
    findings = product_hash_findings(root)
    for name, blob in _pack_blobs(root):
        if (m := EMAIL.search(blob)):
            findings.append(f"{name}: e-mail address {m.group()[:3].decode('utf-8', 'replace')}...")
        try:
            data = json.loads(blob)
        except (ValueError, UnicodeDecodeError):
            continue
        values: list = []
        _identifying_values(data, values)
        findings += [f"{name}: {v}..." for v in values if v.split("=", 1)[1] not in game_ids]
    return sorted(set(findings))
