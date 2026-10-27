#!/usr/bin/env python3
"""The game's own table of store items, read out of the running game: game object (item, token, pack...) -> its
store GUID. Read-only on the game.

Steep keeps Ubisoft store GUIDs as text in a string pool, and records that point at them hold the game object's
id (the resource id in Game Bootstrap Settings) 16 bytes before the pointer:
    [ptr] [u64 object id] [ptr] [ptr -> "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"] ...
The GUIDs are not in the archives (they live in the VMProtect-packed executable), so this is the way to get them.
Owning an item = having its GUID in the inventory: `powder-server admin grant <GUID> 1`, or the overlay's Server
tab -> Grant.

  storemap.py <pid> <Steep folder> [-o out.json] [-g NAME]...   (-g: print only names containing NAME)
  storemap.py <pid> <Steep folder> --item-list items.json       (the space's item list itself, see item_list)

Without a game-data pack, the server serves the catalogue items (those with tags) as free store offers when
`<data>/store_items.json` exists: `storemap.py <pid> <Steep folder> -o data/store_items.json`.
"""

import argparse
import json
import os
import re
import struct
import sys

import numpy as np

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "forge"))
import forge  # noqa: E402

CHUNK = 64 * 1024 * 1024
GUID = re.compile(rb"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\x00")


def regions(pid: int, writable: bool):
    for line in open(f"/proc/{pid}/maps"):
        parts = line.split()
        perms = parts[1]
        path = parts[5] if len(parts) >= 6 else ""
        if not perms.startswith("rw" if writable else "r") or path.startswith("/dev") or path.startswith("["):
            continue
        a, b = (int(x, 16) for x in parts[0].split("-"))
        yield a, b


def chunks(mem, pid: int, writable: bool):
    for start, end in regions(pid, writable):
        off = start
        while off < end:
            n = min(CHUNK, end - off)
            try:
                mem.seek(off)
                buf = mem.read(n)
            except (OSError, ValueError):
                break
            if not buf:
                break
            yield off, buf
            off += len(buf)


def items(game: str) -> dict[int, tuple[int, str]]:
    """Game Bootstrap Settings' resources: id -> (type, name), from the newest copy (DataPC_patch_02 or older)."""
    for p in reversed(forge.patch_stack(os.path.join(game, "DataPC.forge"))):
        if os.path.exists(p):
            fg = forge.Forge(p)
            if 0x800 in fg.by_id:
                df = forge.parse_datafile(fg.read(fg.by_id[0x800]))
                return {r.id: (r.type, r.name) for r in df.resources}
    sys.exit("no Game Bootstrap Settings")


SPACE = b"a20f75db-9175-4df6-b1b3-ba42e291166c\x00"  # Steep's space id


def catalogue(mem, pid: int, names: dict) -> dict[str, tuple[str, list]]:
    """The game's item catalogue: per item a list of tags (std::string records, 48 bytes each, one of them the
    item's own name), then its GUID and the space id (as strings, then again as inline text). Anchored on the
    inline space-id text: the GUID's text sits 0x30 before it, the last tag 0xb0 before it."""

    def string(rec: bytes) -> str | None:
        size, cap = struct.unpack_from("<QQ", rec, 16)
        if cap == 15 and size <= 15:
            return rec[:size].decode("latin1")
        if size > 256 or cap < size:
            return None
        p = struct.unpack_from("<Q", rec)[0]
        try:
            mem.seek(p)
            return mem.read(size).decode("latin1")
        except (OSError, ValueError, OverflowError):
            return None
    out = {}
    for off, buf in chunks(mem, pid, writable=True):
        i = buf.find(SPACE)
        while i >= 0:
            guid = buf[i - 0x30:i - 0x30 + 36]
            if i >= 0x400 and GUID.fullmatch(guid + b"\x00"):
                tags = []
                for k in range(0xb0, 0xb0 + 0x30 * 20, 0x30):
                    s = string(buf[i - k:i - k + 48])
                    if not s or not s.isprintable():
                        break
                    if not GUID.fullmatch(s.encode() + b"\x00"):  # the space id record can sit among them
                        tags.append(s)
                for t in tags:
                    if t in names:
                        out.setdefault(t, (guid.decode(), tags[::-1]))
            i = buf.find(SPACE, i + 1)
    return out


def item_list(mem, pid: int) -> list[dict]:
    """Every record of the game's space item list (the list items/all serves; the game keeps a copy and downloads
    it only when the server's fingerprint differs): {itemId, spaceId, tags}, in memory order. One tag is the game
    object's name, which is how the game ties a GUID to its object."""

    def string(rec: bytes) -> str | None:
        size, cap = struct.unpack_from("<QQ", rec, 16)
        if cap == 15 and size <= 15:
            return rec[:size].decode("latin1")
        if size > 256 or cap < size:
            return None
        try:
            mem.seek(struct.unpack_from("<Q", rec)[0])
            return mem.read(size).decode("latin1")
        except (OSError, ValueError, OverflowError):
            return None
    out, seen = [], set()
    for off, buf in chunks(mem, pid, writable=True):
        i = buf.find(SPACE)
        while i >= 0:
            guid = buf[i - 0x30:i - 0x30 + 36]
            if i >= 0x400 and GUID.fullmatch(guid + b"\x00") and guid not in seen and guid != SPACE[:36]:
                tags = []
                for k in range(0xb0, 0xb0 + 0x30 * 40, 0x30):
                    s = string(buf[i - k:i - k + 48])
                    if not s or not s.isprintable() or GUID.fullmatch(s.encode() + b"\x00"):
                        break
                    tags.append(s)
                if tags:
                    seen.add(guid)
                    out.append({"itemId": guid.decode(), "spaceId": SPACE[:-1].decode(), "tags": tags[::-1]})
            i = buf.find(SPACE, i + 1)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("pid", type=int)
    ap.add_argument("game")
    ap.add_argument("-o", "--out", help="write {name: {guid, id, type}} as JSON")
    ap.add_argument("-g", "--grep", action="append", help="print only names containing this")
    ap.add_argument("--item-list", metavar="OUT", help="only write the space's whole item list ({itemId, spaceId, "
                    "tags} per item) as JSON")
    a = ap.parse_args()
    if a.item_list:
        listed = item_list(open(f"/proc/{a.pid}/mem", "rb", 0), a.pid)
        with open(a.item_list, "w") as f:
            json.dump({"items": listed}, f, indent=1)
        print(f"{len(listed)} items", file=sys.stderr)
        return
    known = items(a.game)
    mem = open(f"/proc/{a.pid}/mem", "rb", 0)
    guids: dict[int, str] = {}
    for off, buf in chunks(mem, a.pid, writable=False):
        for m in GUID.finditer(buf):
            guids[off + m.start()] = m.group()[:36].decode()
    print(f"{len(guids)} GUID strings in memory", file=sys.stderr)
    targets = np.array(sorted(guids), dtype=np.uint64)
    found: dict[int, set] = {}
    for off, buf in chunks(mem, a.pid, writable=True):
        n = len(buf) // 8 * 8
        arr = np.frombuffer(buf[:n], dtype=np.uint64)
        hits = np.nonzero(np.isin(arr, targets))[0]
        for h in hits:
            if h < 2:
                continue
            oid = int(arr[h - 2])
            if oid in known:
                found.setdefault(oid, set()).add(guids[int(arr[h])])
    out = {}
    for oid, gs in sorted(found.items(), key=lambda kv: known[kv[0]][1]):
        typ, name = known[oid]
        out[name] = {"guid": sorted(gs)[0] if len(gs) == 1 else sorted(gs), "id": f"{oid:#x}", "type": f"{typ:#x}"}
    print(f"{len(out)} progression objects with a store GUID", file=sys.stderr)
    names = {n: (oid, typ) for oid, (typ, n) in known.items()}
    for name, (guid, tags) in catalogue(mem, a.pid, names).items():
        if name not in out:
            oid, typ = names[name]
            out[name] = {"guid": guid, "id": f"{oid:#x}", "type": f"{typ:#x}", "tags": tags}
    print(f"{len(out)} game objects with a store GUID in all", file=sys.stderr)
    for name, v in out.items():
        if not a.grep or any(g.lower() in name.lower() for g in a.grep):
            print(f"{name:60} {v['guid']}  {v['id']} {v['type']}")
    if a.out:
        with open(a.out, "w") as f:
            json.dump(out, f, indent=1)


if __name__ == "__main__":
    main()
