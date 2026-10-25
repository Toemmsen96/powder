"""What a game-data pack may contain, checked again whenever one is installed, loaded or zipped.

A pack may come from someone else (a .zip passed around), and its manifest only proves that its files were not
changed since whoever made it wrote it. So the builder's allow-lists (build.py) are applied again here, and
what would go to the game or to Ubisoft Connect as it is gets a strict check:

- http.json / connect/http.json: only GET replies for the allow-listed paths, on the redirected hostnames, with
  a JSON body; besides those Connect's achievement definitions (a zip of '<lang>_loc.txt' texts and '<id>.png'
  icons, size-capped) and its free-games list (UTF-8 text). They are served as JSON / zip / plain text whatever
  content type the pack names, so a pack cannot serve a page or a script.
- rmc.json: only the allow-listed NEX methods, bodies hex.
- connect/demux.json: only the ids of Steep's products are used (each must be one of builtin/product_table.py's):
  the product configurations (YAML: executables, working directory, registry paths) Connect gets are the
  server's own (builtin/products.py), never the pack's. The patch info keeps only its numbers and release
  name (the patch URL is the server's).
- offers.json / actions.json: the shapes the server reads; actions without player state.

A pack that fails is refused as a whole (PackError naming what is wrong): the server then runs on its built-in
replies, as without a pack.
"""
from __future__ import annotations

import base64
import binascii
import io
import json
import re
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

from ..builtin.product_table import KINDS
from ..fixtures.http import HttpFixture
from ..hostnames import REDIRECT_HOSTS
from ..transport.demux import pb
from ..transport.demux.connection import PATCH_BASE_URL
from .build import ACTION_PLAYER_FIELDS, CONNECT_ALLOW, GAME_ALLOW, RMC_ALLOW, _allowed
from .pack import PackError

# Connect's achievement definitions (the one zip a pack may hold) and its free-games list (text)
ACHIEVEMENTS_PATH = re.compile(r"/orbit/uplay_launcher_3_0/assets/[0-9a-f]+\.zip")
TEXT_PATHS = re.compile(r"/orbit/uplay_launcher_14_0/free_games/latest\.txt")
ACHIEVEMENT_MEMBER = re.compile(r"[A-Za-z]{2,3}(-[A-Za-z0-9]{2,4})?_loc\.txt|\d{1,6}\.png")
MAX_ZIP_MEMBERS = 4096
MAX_ZIP_BYTES = 64 << 20                    # uncompressed, all members (Steep's: a few MB of icons)
JSON_TYPES = ("application/json", "text/json", "text/plain")
JSON_TYPE = "application/json"


@dataclass
class ValidPack:
    """The checked contents of a pack, as the server uses them."""
    http: list[HttpFixture] = field(default_factory=list)
    connect_http: list[HttpFixture] = field(default_factory=list)
    rmc: dict = field(default_factory=dict)
    offers: list[dict] = field(default_factory=list)
    actions: list[dict] = field(default_factory=list)
    patch_info: bytes | None = None
    product_ids: list[int] = field(default_factory=list)


def read_zip(data: bytes, member: re.Pattern = ACHIEVEMENT_MEMBER) -> dict[str, bytes]:
    """The members of a zip (achievement definitions) by name, capped in count and uncompressed size, only names
    matching `member`. PackError otherwise. (zipfile never inflates a member beyond its declared size.)"""
    try:
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            infos = [i for i in z.infolist() if not i.is_dir()]
            if len(infos) > MAX_ZIP_MEMBERS:
                raise PackError(f"the achievements zip has {len(infos)} files (at most {MAX_ZIP_MEMBERS})")
            total = sum(i.file_size for i in infos)
            if total > MAX_ZIP_BYTES:
                raise PackError(f"the achievements zip unpacks to {total} bytes (at most {MAX_ZIP_BYTES})")
            odd = [i.filename for i in infos if not member.fullmatch(i.filename)]
            if odd:
                raise PackError(f"unexpected file in the achievements zip: {odd[0][:80]}")
            return {i.filename: z.read(i) for i in infos}
    except (zipfile.BadZipFile, zipfile.LargeZipFile, OSError, EOFError, NotImplementedError) as e:
        raise PackError(f"the achievements zip is not a valid zip ({e})") from None


def _media_type(content_type) -> str:
    return str(content_type or "").split(";", 1)[0].strip().lower()


def _http(raw, rules, name: str, connect: bool) -> list[HttpFixture]:
    if not isinstance(raw, list):
        raise PackError(f"{name}: not a list of replies")
    out = []
    for n, e in enumerate(raw):
        where = f"{name} entry {n}"
        if not isinstance(e, dict):
            raise PackError(f"{where}: not an object")
        method, host, path, status = e.get("method"), str(e.get("host", "")).lower(), e.get("path"), e.get("status")
        if not isinstance(path, str) or not _allowed(rules, method, path):
            raise PackError(f"{where}: {method} {str(path)[:80]} is not one of the replies a pack may hold")
        if host not in REDIRECT_HOSTS:
            raise PackError(f"{where}: host {host[:60]} is not one this server answers for")
        if not isinstance(status, int) or isinstance(status, bool) or not 200 <= status <= 599:
            raise PackError(f"{where}: status {status!r}")
        try:
            body = base64.b64decode(e.get("body_b64", ""), validate=True)
        except (binascii.Error, ValueError, TypeError):
            raise PackError(f"{where}: body is not base64") from None
        base = path.split("?", 1)[0]
        if connect and ACHIEVEMENTS_PATH.fullmatch(base):
            read_zip(body)
            content_type = "application/zip"
        elif connect and TEXT_PATHS.fullmatch(base):
            try:
                body.decode("utf-8")
            except UnicodeDecodeError:
                raise PackError(f"{where}: {base} is not text") from None
            content_type = "text/plain; charset=utf-8"
        else:
            try:
                json.loads(body)
            except (ValueError, UnicodeDecodeError):
                raise PackError(f"{where}: {base[:80]} does not reply JSON") from None
            recorded = e.get("content_type")
            content_type = recorded if _media_type(recorded) in JSON_TYPES else JSON_TYPE
        out.append(HttpFixture(method, host, path, status, content_type, body))
    return out


def _rmc(raw) -> dict:
    if not isinstance(raw, dict):
        raise PackError("rmc.json: not an object")
    for method, rows in raw.items():
        if method not in RMC_ALLOW:
            raise PackError(f"rmc.json: {str(method)[:80]} is not one of the NEX replies a pack may hold")
        if not isinstance(rows, list) or not all(isinstance(r, dict) and isinstance(r.get("params_sha1"), str)
                                                 and isinstance(r.get("body"), str) for r in rows):
            raise PackError(f"rmc.json: {method}: not a list of recorded replies")
        for r in rows:
            try:
                bytes.fromhex(r["body"])
            except ValueError:
                raise PackError(f"rmc.json: {method}: a body is not hex") from None
    return raw


def _patch_info(b64) -> bytes | None:
    """Connect's patch info {1 ok, 2 release name, 3, 4 patch base URL, 5 latest version, 6, 7}, rebuilt from the
    pack's: its numbers and a plain release name; the patch URL is always the server's own (Connect never patches
    from it, the latest version is always its own, but a pack must not name where it would)."""
    if not b64:
        return None
    try:
        fields = pb.decode(base64.b64decode(b64, validate=True))
    except (binascii.Error, ValueError, TypeError):
        raise PackError("connect/demux.json: patch_info is not a patch info message") from None
    out = []
    for number, value, _ in fields:
        if number in (1, 3, 5, 6, 7) and isinstance(value, int):
            out.append((number, value))
        elif number == 2:
            ok = isinstance(value, bytes) and re.fullmatch(rb"[\w.\- ]{0,64}", value)
            out.append((2, value if ok else b"0"))
        elif number == 4:
            out.append((4, PATCH_BASE_URL))
    return pb.encode(out)


def _product_ids(raw) -> list[int]:
    ids = []
    for n, b64 in enumerate(raw or []):
        try:
            fields = pb.decode(base64.b64decode(b64, validate=True))
        except (binascii.Error, ValueError, TypeError):
            raise PackError(f"connect/demux.json: product {n} is not a product entry") from None
        pid = pb.first(fields, 1)
        if not isinstance(pid, int) or pid not in KINDS:
            raise PackError(f"connect/demux.json: product {pid!r} is not one of Steep's")
        ids.append(pid)
    return ids


def _load(root: Path, name: str, default):
    path = root / name
    if not path.exists():
        return default
    if path.is_symlink() or not path.is_file():
        raise PackError(f"{name} is not a plain file")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (ValueError, UnicodeDecodeError):
        raise PackError(f"{name} is not JSON") from None


def validated(root: Path) -> ValidPack:
    """The pack at `root`, checked (see the module doc); PackError says what is wrong."""
    try:
        demux = _load(root, "connect/demux.json", {})
        offers = _load(root, "offers.json", {"offers": []})
        actions = _load(root, "actions.json", {"actions": []})
        if not isinstance(demux, dict) or not isinstance(offers, dict) or not isinstance(actions, dict):
            raise PackError("connect/demux.json, offers.json and actions.json must be objects")
        offer_list, action_list = offers.get("offers", []), actions.get("actions", [])
        if not isinstance(offer_list, list) or not all(isinstance(o, dict) and isinstance(o.get("offerId"), str)
                                                       for o in offer_list):
            raise PackError("offers.json: not a list of offers")
        if not isinstance(action_list, list) or not all(isinstance(a, dict) for a in action_list):
            raise PackError("actions.json: not a list of actions")
        if any(k in a for a in action_list for k in ACTION_PLAYER_FIELDS):
            raise PackError("actions.json: holds player state (profileId / value)")
        if not isinstance(demux.get("products", []), list):
            raise PackError("connect/demux.json: products is not a list")
        return ValidPack(http=_http(_load(root, "http.json", []), GAME_ALLOW, "http.json", connect=False),
                         connect_http=_http(_load(root, "connect/http.json", []), CONNECT_ALLOW,
                                            "connect/http.json", connect=True),
                         rmc=_rmc(_load(root, "rmc.json", {})), offers=offer_list, actions=action_list,
                         patch_info=_patch_info(demux.get("patch_info")),
                         product_ids=_product_ids(demux.get("products")))
    except PackError as e:
        raise PackError(f"the game-data pack at {root} is refused: {e}") from None
