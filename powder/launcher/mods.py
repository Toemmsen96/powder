"""Client mods: DLLs a server offers to load into the game, next to the TLS helper.

The server lists them at GET /_powder/mods and serves each file at GET /_powder/mods/<name>/<path>, over
its HTTPS port, checked against its CA as pinned for that server (like the account calls; no redirects
followed). Trust model: a server must never be able to put code on a player's PC unasked, so
  - client mods are off unless launcher.toml says `[client_mods] enabled = true`,
  - only the mods named in `allow` are considered (exact names, no wildcard),
  - each one is used only once the player approved it for this server, shown with its files, sizes and
    SHA-256 (refusing is the first choice): the approval pins the files (path + sha256) and the inject list
    (settings.json client_mod_approvals, under the server's pinned CA fingerprint, so another server, or
    another CA at the same address, asks again); afterwards any change asks again,
  - every file is checked for size and sha256 after download and again right before it is injected,
  - files land only in <data_dir>/client-mods/<server>/<mod>/: paths from the server that are absolute,
    contain "..", a drive letter, a backslash, a colon or a Windows device name are rejected; the list is
    bounded (MAX_MODS mods, MAX_FILES files and MAX_MOD_BYTES per mod), and its texts are cleaned of
    control characters and cut before they are shown.
Game files are never touched.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import ssl
import urllib.error
import urllib.parse
from dataclasses import dataclass
from pathlib import Path, PurePosixPath

from . import server

API = 1
MAX_INDEX = 1 << 20                      # bytes of /_powder/mods we read at most
MAX_FILE = 256 << 20                     # a single mod file at most
MAX_MOD_BYTES = 512 << 20                # all files of one mod at most
MAX_FILES = 256                          # files per mod
MAX_MODS = 64                            # mods in the list
MAX_NAME, MAX_VERSION, MAX_DESCRIPTION, MAX_PATH = 64, 40, 200, 240
SHA256 = re.compile(r"^[0-9a-f]{64}$")
RESERVED = {"con", "prn", "aux", "nul", "conin$", "conout$"} | {f"{d}{i}" for d in ("com", "lpt") for i in range(10)}

# what the launcher does with an offered mod
OFF = "off"                 # client mods are off in launcher.toml
NOT_ALLOWED = "not allowed" # not in client_mods.allow
NEW = "new"                 # allowed, not approved for this server yet: asks
ACTIVE = "active"           # allowed and approved, files as pinned
CHANGED = "changed"         # allowed, but its files differ from the approval: asks again


class ModError(Exception):
    """A mod (or the index) the launcher refuses; the text is for the log and the player."""


@dataclass(frozen=True)
class ModFile:
    path: str
    size: int
    sha256: str


@dataclass(frozen=True)
class ClientMod:
    name: str
    version: str
    description: str
    files: tuple[ModFile, ...]
    inject: tuple[str, ...]

    def pin(self) -> dict:
        """What is pinned on first use: the exact files and what gets loaded, in order."""
        return {"version": self.version, "files": {f.path: f.sha256 for f in self.files},
                "inject": list(self.inject)}


def clean(text, limit: int) -> str:
    """A server-given text as it may be shown: printable characters only (no control or bidi characters),
    whitespace collapsed, at most `limit` characters."""
    text = " ".join("".join(c if c.isprintable() else " " for c in str(text)).split())
    return text if len(text) <= limit else text[:limit - 3] + "..."


# ---- path safety ------------------------------------------------------------------------------------------
def safe_segment(seg: str) -> str:
    """One path component that means the same on Windows and POSIX and stays where it is put."""
    if not seg or seg in (".", ".."):
        raise ModError(f"bad path component {seg!r}")
    if any(c in seg for c in '\\/:*?"<>|') or any(ord(c) < 32 for c in seg):
        raise ModError(f"bad character in {seg!r}")
    if seg[-1] in ". ":                                  # Windows drops them: "a.dll." is "a.dll"
        raise ModError(f"{seg!r} ends in a dot or space")
    if seg.split(".")[0].strip().lower() in RESERVED:
        raise ModError(f"{seg!r} is a Windows device name")
    return seg


def safe_rel_path(path: str) -> PurePosixPath:
    """A server-given relative path ("bin/x.dll"), or ModError if it could leave the mod's folder."""
    if not isinstance(path, str) or not path:
        raise ModError("empty path")
    if len(path) > MAX_PATH:
        raise ModError(f"path {clean(path, 60)!r} is longer than {MAX_PATH} characters")
    if path.startswith(("/", "\\")) or "\\" in path or ":" in path:
        raise ModError(f"path {path!r} is absolute or uses a drive, backslash or colon")
    parts = path.split("/")
    for seg in parts:
        safe_segment(seg)
    return PurePosixPath(*parts)


def target(root: Path, path: str) -> Path:
    """Where `path` of a mod goes under `root` (the mod's folder); never outside it (also not via links)."""
    rel = safe_rel_path(path)
    dest = root.joinpath(*rel.parts)
    if not dest.resolve().is_relative_to(root.resolve()):
        raise ModError(f"path {path!r} leaves the mod's folder")
    return dest


# ---- the index ---------------------------------------------------------------------------------------------
def parse_mod(raw) -> ClientMod:
    if not isinstance(raw, dict):
        raise ModError("mod entry is not an object")
    name = raw.get("name")
    if not isinstance(name, str):
        raise ModError("mod without a name")
    if len(name) > MAX_NAME:
        raise ModError(f"mod name {clean(name, 40)!r} is longer than {MAX_NAME} characters")
    safe_segment(name)
    files, seen = [], set()
    listed = raw.get("files") or []
    if not isinstance(listed, list) or len(listed) > MAX_FILES:
        raise ModError(f"{name}: more than {MAX_FILES} files, or not a list")
    for f in listed:
        if not isinstance(f, dict):
            raise ModError(f"{name}: file entry is not an object")
        path = str(safe_rel_path(f.get("path")))
        size, sha = f.get("size"), str(f.get("sha256", "")).lower()
        if not isinstance(size, int) or isinstance(size, bool) or not 0 <= size <= MAX_FILE:
            raise ModError(f"{name}: {path} has no valid size")
        if not SHA256.match(sha):
            raise ModError(f"{name}: {path} has no valid sha256")
        if path.lower() in seen:                           # Windows paths ignore case
            raise ModError(f"{name}: {path} is listed twice")
        seen.add(path.lower())
        files.append(ModFile(path, size, sha))
    if not files:
        raise ModError(f"{name}: no files")
    if sum(f.size for f in files) > MAX_MOD_BYTES:
        raise ModError(f"{name}: more than {MAX_MOD_BYTES >> 20} MiB in all")
    inject = raw.get("inject") or []
    if not isinstance(inject, list):
        raise ModError(f"{name}: inject is not a list")
    by_path = {f.path for f in files}
    for p in inject:
        if not isinstance(p, str) or str(safe_rel_path(p)) not in by_path:
            raise ModError(f"{name}: inject {p!r} is not one of its files")
        if not p.lower().endswith(".dll"):
            raise ModError(f"{name}: inject {p!r} is not a .dll")
    return ClientMod(name, clean(raw.get("version", ""), MAX_VERSION), clean(raw.get("description", ""),
                     MAX_DESCRIPTION), tuple(files), tuple(str(safe_rel_path(p)) for p in inject))


def parse_index(data) -> tuple[list[ClientMod], list[str]]:
    """(usable mods, problems with the others). ModError if the whole index can't be used."""
    if not isinstance(data, dict) or data.get("api") != API or not isinstance(data.get("mods"), list):
        raise ModError(f"the server's mod list is not in a format this launcher knows (api {API})")
    mods, problems, names = [], [], set()
    if len(data["mods"]) > MAX_MODS:
        problems.append(f"{len(data['mods']) - MAX_MODS} mods beyond the first {MAX_MODS} ignored")
    for raw in data["mods"][:MAX_MODS]:
        try:
            mod = parse_mod(raw)
        except ModError as e:
            problems.append(str(e))
            continue
        if mod.name.lower() in names:
            problems.append(f"{mod.name}: listed twice")
            continue
        names.add(mod.name.lower())
        mods.append(mod)
    return mods, problems


# ---- consent ------------------------------------------------------------------------------------------------
def decide(mods: list[ClientMod], enabled: bool, allow, pins: dict) -> dict[str, str]:
    """What happens to each offered mod (OFF / NOT_ALLOWED / NEW / ACTIVE / CHANGED), from launcher.toml's
    switch and allow list and the approvals for this server's CA (`pins`: name -> approved pin)."""
    allow = set(allow)
    out = {}
    for m in mods:
        if not enabled:
            out[m.name] = OFF
        elif m.name not in allow:
            out[m.name] = NOT_ALLOWED
        elif m.name not in pins:
            out[m.name] = NEW
        else:
            out[m.name] = ACTIVE if same_pin(pins[m.name], m.pin()) else CHANGED
    return out


def same_pin(old: dict, new: dict) -> bool:
    """The version string alone may change; the files and what is loaded may not."""
    return isinstance(old, dict) and old.get("files") == new["files"] and old.get("inject") == new["inject"]


def changes(old: dict, mod: ClientMod) -> str:
    """For the player: what differs from the approved version."""
    before, after = dict(old.get("files") or {}), mod.pin()["files"]
    parts = [f"{p} changed" for p in after if p in before and before[p] != after[p]]
    parts += [f"{p} added" for p in after if p not in before]
    parts += [f"{p} removed" for p in before if p not in after]
    if old.get("inject") != list(mod.inject):
        parts.append("load order " + " -> ".join(mod.inject or ["nothing"]))
    return clean(", ".join(parts), 400) or "nothing"


def listing(mod: ClientMod) -> str:
    """For the player to check (and copy): each file's SHA-256, size and path, and what gets loaded."""
    lines = [f"{f.sha256}  {f.size:>10}  {f.path}" for f in mod.files]
    return "\n".join(lines + [f"loads, in order: {', '.join(mod.inject) or 'nothing'}"])


# ---- download -----------------------------------------------------------------------------------------------
def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def intact(path: Path, f: ModFile) -> bool:
    try:
        return path.is_file() and not path.is_symlink() and path.stat().st_size == f.size \
            and sha256_of(path) == f.sha256
    except OSError:
        return False


def sync(mod: ClientMod, root: Path, fetch) -> list[str]:
    """Bring <root> (this mod's folder) to the listed files: download what is missing or different via
    `fetch(url_path, max_bytes) -> bytes`, check each, and remove files that are no longer listed. Returns
    the paths downloaded. ModError if a file can't be had intact (what was there stays as it was)."""
    root.mkdir(parents=True, exist_ok=True)
    got = []
    for f in mod.files:
        dest = target(root, f.path)
        if intact(dest, f):
            continue
        quoted = "/".join(urllib.parse.quote(s, safe="") for s in (mod.name, *f.path.split("/")))
        data = fetch(f"/_powder/mods/{quoted}", f.size)
        if len(data) != f.size:
            raise ModError(f"{mod.name}: {f.path} is {len(data)} bytes, the list says {f.size}")
        if hashlib.sha256(data).hexdigest() != f.sha256:
            raise ModError(f"{mod.name}: {f.path} does not match its sha256")
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_name(dest.name + ".part")
        tmp.write_bytes(data)
        os.replace(tmp, dest)
        got.append(f.path)
    keep = {target(root, f.path).resolve() for f in mod.files}
    for p in sorted(root.rglob("*"), reverse=True):        # files first, then emptied folders
        if p.is_file() or p.is_symlink():
            if p.resolve() not in keep:
                p.unlink()
        elif p.is_dir() and not any(p.iterdir()):
            p.rmdir()
    return got


def dlls(mod: ClientMod, root: Path) -> list[Path]:
    """Full paths of the mod's DLLs to load, in order, each checked again just before (ModError if not
    intact any more)."""
    by_path = {f.path: f for f in mod.files}
    out = []
    for p in mod.inject:
        dest = target(root, p)
        if not intact(dest, by_path[p]):
            raise ModError(f"{mod.name}: {p} changed on disk since it was downloaded")
        out.append(dest.resolve())
    return out


def remove_unused(server_dir: Path, keep: set[str]) -> None:
    """Delete the folders of mods no longer used from this server (only inside `server_dir`)."""
    if not server_dir.is_dir():
        return
    for p in server_dir.iterdir():
        if p.is_dir() and not p.is_symlink() and p.name not in keep:
            shutil.rmtree(p, ignore_errors=True)


# ---- HTTP ---------------------------------------------------------------------------------------------------
class Http:
    """GETs on the server's HTTPS port, checked against its pinned CA (reached by address: no name check),
    no redirects."""
    def __init__(self, address: str, port: int, ca: Path, timeout: float = 30):
        self.base = f"https://{address}:{port}"
        self.timeout = timeout
        self.ctx = ssl.create_default_context(cafile=str(ca))
        self.ctx.check_hostname = False

    def get(self, path: str, max_bytes: int) -> bytes:
        """The body; ModError on HTTP errors, unreachable server or a body over `max_bytes`."""
        try:
            with server.open_url(self.base + path, self.ctx, self.timeout) as r:
                data = r.read(max_bytes + 1)
        except urllib.error.HTTPError as e:
            raise ModError(f"{path}: HTTP {e.code}") from e
        except OSError as e:
            raise ModError(f"{path}: {e}") from e
        if len(data) > max_bytes:
            raise ModError(f"{path}: larger than expected")
        return data


def fetch_index(get) -> tuple[list[ClientMod], list[str]] | None:
    """The mods the server offers (see parse_index); None if it has no mod list (an older server)."""
    try:
        data = get("/_powder/mods", MAX_INDEX)
    except ModError as e:
        if "HTTP 404" in str(e):
            return None
        raise
    try:
        return parse_index(json.loads(data))
    except ValueError as e:
        raise ModError(f"the server's mod list is not JSON ({e})") from e
