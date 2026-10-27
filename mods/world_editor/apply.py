#!/usr/bin/env python3
"""World editor, session step: the server's stored world edits -> edited world cells, and the object index the
overlay panel lists (see README.md).

    apply.py session                 what linux/powder-wrap runs (session.sh) at each game start, from the env:
                                     POWDER_STEEP_DIR, POWDER_DATA_DIR, POWDER_HTTPS_PORT, POWDER_MOD_FORGE,
                                     POWDER_MOD_RUNTIME; WORLD_EDITOR_MAPS, WORLD_EDITOR_ADDS=0 (no copies)
    apply.py generate [--edits FILE | --server URL --cacert CA] [--steep DIR] [-o DIR]
                                     the cells only (default generated/DataPC_STP_<Map>/), e.g. from edits.json
    apply.py index [--maps Alpes,...] [--steep DIR] [--out DIR]
                                     the panel's index files (<Map>.tsv), cached in ~/.cache/powder/world_editor

Edits come from the running server (GET /_powder/world_editor/edits, answered by mod.lua); a copy of the last list
fetched is kept in <data dir>/world_editor/edits.json, which is used when the server does not answer (e.g. the
Python server, which runs no Lua mods). Game data is only read (tools/forge/world.py). In a session the cells go
into powder-wrap's POWDER_MOD_FORGE folder, which it packs into the session's <Map>_patch_03.forge and deletes:
not into client/, whose files the server publishes to launchers (they are Steep's data and change every session).
Python 3.10+, stdlib only.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
WORLD = os.path.join(HERE, "..", "..", "tools", "forge", "world.py")
MAPS = ("Alpes", "Japan", "Alaska", "WhiteRoom")
HOST = "public-ubiservices.ubi.com"   # the name the server's certificate carries
PATH = "/_powder/world_editor/edits"
DEFAULT_STEEP = os.path.expanduser("~/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/"
                                   "Ubisoft Game Launcher/games/Steep")


def log(msg: str) -> None:
    print(f"world_editor: {msg}", file=sys.stderr, flush=True)


def fetch(port: int, cafile: str, host: str = "127.0.0.1", timeout: float = 10.0) -> dict:
    """The edit list from the Powder server at host:port (TLS checked against the server's own CA)."""
    ctx = ssl.create_default_context(cafile=cafile)
    with socket.create_connection((host, port), timeout=timeout) as raw, \
            ctx.wrap_socket(raw, server_hostname=HOST) as s:
        s.sendall(f"GET {PATH} HTTP/1.1\r\nHost: {HOST}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
                  .encode())
        data = b""
        while chunk := s.recv(65536):
            data += chunk
            if len(data) > 16 << 20:
                raise ValueError("reply too large")
    head, _, body = data.partition(b"\r\n\r\n")
    status = head.split(b" ", 2)[1:2]
    if status != [b"200"]:
        raise ValueError(f"HTTP {status[0].decode() if status else '?'} (is mods/world_editor/mod.lua loaded?)")
    if b"transfer-encoding: chunked" in head.lower():
        body = dechunk(body)
    d = json.loads(body)
    if not isinstance(d, dict) or not isinstance(d.get("edits"), list):
        raise ValueError("not an edit list")
    return d


def dechunk(b: bytes) -> bytes:
    out = b""
    while b:
        size, _, rest = b.partition(b"\r\n")
        n = int(size.split(b";")[0], 16)
        if n == 0:
            break
        out, b = out + rest[:n], rest[n + 2:]
    return out


def load_edits(a, data_dir: str | None) -> list[dict]:
    """The edit list: --edits FILE, else the server (the snapshot refreshed), else the last snapshot."""
    if getattr(a, "edits", None):
        with open(a.edits, encoding="utf-8") as f:
            d = json.load(f)
        return d.get("edits", []) if isinstance(d, dict) else d
    snap = os.path.join(data_dir, "world_editor", "edits.json") if data_dir else None
    port = getattr(a, "port", None)
    cafile = getattr(a, "cacert", None) or (os.path.join(data_dir, "tls", "ca.pem") if data_dir else None)
    if port and cafile:
        try:
            d = fetch(port, cafile, getattr(a, "host", None) or "127.0.0.1")
            if snap:
                os.makedirs(os.path.dirname(snap), exist_ok=True)
                tmp = snap + ".tmp"
                with open(tmp, "w", encoding="utf-8") as f:
                    json.dump(d, f, indent=1)
                os.replace(tmp, snap)
            log(f"{len(d['edits'])} edit(s) from the server")
            return d["edits"]
        except (OSError, ValueError, ssl.SSLError) as ex:
            log(f"server did not give the edits ({ex})" + (": using the last copy" if snap and os.path.exists(snap)
                                                            else ""))
    if snap and os.path.exists(snap):
        with open(snap, encoding="utf-8") as f:
            return json.load(f).get("edits", [])
    return []


def map_of(e: dict) -> str | None:
    m = str(e.get("map", ""))
    return m if m in MAPS else None


def generate(edits: list[dict], steep: str, forge_dir: str) -> int:
    """Write the edited cells of every map with edits into <forge_dir>/DataPC_STP_<Map>/ (all regenerated)."""
    for m in MAPS:
        shutil.rmtree(os.path.join(forge_dir, f"DataPC_STP_{m}"), ignore_errors=True)
    if os.environ.get("WORLD_EDITOR_ADDS") == "0":   # copies are experimental: moves/turns/scales/removes only
        kept = [e for e in edits if e.get("op") != "add" and "of" not in e]
        if len(kept) != len(edits):
            log(f"WORLD_EDITOR_ADDS=0: {len(edits) - len(kept)} edit(s) of added objects left out")
        edits = kept
    maps = sorted({m for e in edits if (m := map_of(e))})
    if not maps:
        log("no world edits: nothing to build")
        return 0
    fd, edits_file = tempfile.mkstemp(prefix="world_editor-", suffix=".json")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump({"edits": edits}, f)
    failed = 0
    for m in maps:
        base = os.path.join(steep, f"DataPC_STP_{m}.forge")
        if not os.path.exists(base):
            log(f"{m}: no {base}: its edits are skipped")
            continue
        out = os.path.join(forge_dir, f"DataPC_STP_{m}")
        r = subprocess.run([sys.executable, WORLD, "apply", base, edits_file, "-o", out],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        for line in r.stdout.splitlines():
            if "is a powder-wrap session's" not in line:
                log(f"{m}: {line.strip()}")
        if r.returncode:
            failed += 1
            shutil.rmtree(out, ignore_errors=True)   # never half a map
            log(f"{m}: world.py apply failed (exit {r.returncode}): this map stays as shipped")
    os.remove(edits_file)
    return 1 if failed else 0


def signature(steep: str, m: str) -> str:
    """The map's archives (name, size, time) as the index was made from: a patch changes it."""
    names = sorted(n for n in os.listdir(steep) if n.startswith(f"DataPC_STP_{m}") and n.endswith(".forge")
                   and "_patch_03" not in n)
    return ";".join(f"{n}:{os.path.getsize(os.path.join(steep, n))}:{int(os.path.getmtime(os.path.join(steep, n)))}"
                    for n in names)


def index(maps: list[str], steep: str, out: str | None) -> int:
    """Build (or reuse) each map's index and copy it to `out`."""
    cache = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "powder", "world_editor")
    os.makedirs(cache, exist_ok=True)
    failed = 0
    for m in maps:
        base = os.path.join(steep, f"DataPC_STP_{m}.forge")
        if not os.path.exists(base):
            log(f"{m}: no {base}: no index")
            continue
        tsv, sig_file = os.path.join(cache, f"{m}.tsv"), os.path.join(cache, f"{m}.sig")
        sig = signature(steep, m)
        try:
            with open(sig_file, encoding="utf-8") as f:
                fresh = f.read() == sig and os.path.exists(tsv)
        except OSError:
            fresh = False
        if not fresh:
            t = time.time()
            log(f"{m}: indexing the map's objects (once per game version; ~20 s for an open-world map)")
            r = subprocess.run([sys.executable, WORLD, "index", base, "-o", tsv], stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, text=True)
            if r.returncode:
                failed += 1
                log(f"{m}: world.py index failed: {r.stdout.strip()[-400:]}")
                continue
            with open(sig_file, "w", encoding="utf-8") as f:
                f.write(sig)
            log(f"{m}: indexed in {time.time() - t:.0f} s")
        if out:
            os.makedirs(out, exist_ok=True)
            shutil.copyfile(tsv, os.path.join(out, f"{m}.tsv"))
    return 1 if failed else 0


def cmd_session(a) -> int:
    steep = os.environ.get("POWDER_STEEP_DIR") or DEFAULT_STEEP
    data_dir = os.environ.get("POWDER_DATA_DIR")
    a.port = int(os.environ.get("POWDER_HTTPS_PORT") or 443)
    edits = load_edits(a, data_dir)
    forge_dir = os.environ.get("POWDER_MOD_FORGE")
    if not forge_dir:
        log("no POWDER_MOD_FORGE (an older powder-wrap?): nothing to build into")
        return 1
    rc = generate(edits, steep, forge_dir)
    runtime = os.environ.get("POWDER_MOD_RUNTIME")
    if runtime:
        maps = [m for m in (os.environ.get("WORLD_EDITOR_MAPS") or ",".join(MAPS)).split(",") if m in MAPS]
        rc |= index(maps, steep, runtime)
    else:
        log("no overlay this session (POWDER_OVERLAY=1): the panel's index is not deployed")
    return rc


def cmd_generate(a) -> int:
    if a.server:
        hostport = a.server.split("//", 1)[-1].split("/", 1)[0]
        a.host, _, port = hostport.rpartition(":") if ":" in hostport else (hostport, "", "443")
        a.port = int(port)
    return generate(load_edits(a, a.data), a.steep, a.out)


def cmd_index(a) -> int:
    return index([m for m in a.maps.split(",") if m in MAPS], a.steep, a.out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)
    p = sp.add_parser("session", help="powder-wrap's session step (settings from the environment)")
    p.set_defaults(fn=cmd_session)
    p = sp.add_parser("generate", help="write the edited cells (by hand, e.g. to check them with world.py show)")
    p.add_argument("-o", "--out", default=os.path.join(HERE, "generated"), help="default: generated/ (gitignored)")
    p.add_argument("--edits", help="an edits JSON (the server's list or edits.json) instead of asking the server")
    p.add_argument("--server", help="e.g. https://127.0.0.1:18443 (any address of the server; TLS name checked)")
    p.add_argument("--cacert", help="the server's CA (default <data>/tls/ca.pem)")
    p.add_argument("--data", help="the server's data folder (for the CA and the edits.json copy)")
    p.add_argument("--steep", default=os.environ.get("POWDER_STEEP_DIR") or DEFAULT_STEEP)
    p.set_defaults(fn=cmd_generate)
    p = sp.add_parser("index", help="build the panel's index files")
    p.add_argument("--maps", default=",".join(MAPS))
    p.add_argument("--steep", default=os.environ.get("POWDER_STEEP_DIR") or DEFAULT_STEEP)
    p.add_argument("--out", help="also copy them here")
    p.set_defaults(fn=cmd_index)
    a = ap.parse_args()
    return a.fn(a)


if __name__ == "__main__":
    sys.exit(main())
