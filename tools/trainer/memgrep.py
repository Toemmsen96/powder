#!/usr/bin/env python3
"""Read-only byte search in a running game's memory (/proc/<pid>/mem), for finding where the game keeps data
(e.g. the store GUIDs of items). Only reads; never writes.

  memgrep.py <pid> [--hex HEX]... [--text TEXT]... [--guid UUID]... [--u64 0x...]... [--context N] [--max N]

--guid searches a UUID as lower/upper-case text and as 16 bytes in both byte orders (RFC and Windows GUID).
Prints `address  region  pattern-label` and the bytes around each hit.
"""

import argparse
import struct
import uuid

CHUNK = 64 * 1024 * 1024


def regions(pid: int):
    for line in open(f"/proc/{pid}/maps"):
        parts = line.split()
        if len(parts) < 5 or not parts[1].startswith("r"):
            continue
        path = parts[5] if len(parts) >= 6 else ""
        if path in ("[vvar]", "[vsyscall]", "[vdso]") or path.startswith("/dev"):
            continue
        a, b = (int(x, 16) for x in parts[0].split("-"))
        yield a, b, path or parts[1]


def patterns(a) -> dict[bytes, str]:
    out = {}
    for h in a.hex or []:
        out[bytes.fromhex(h)] = f"hex {h}"
    for t in a.text or []:
        out[t.encode()] = f"text {t}"
        out[t.encode("utf-16-le")] = f"utf16 {t}"
    for g in a.guid or []:
        u = uuid.UUID(g)
        out[str(u).encode()] = f"guid-text {g}"
        out[str(u).upper().encode()] = f"guid-TEXT {g}"
        out[str(u).encode("utf-16-le")] = f"guid-utf16 {g}"
        out[u.bytes] = f"guid-bytes {g}"
        out[u.bytes_le] = f"guid-bytes-le {g}"
    for v in a.u64 or []:
        out[struct.pack("<Q", int(v, 0))] = f"u64 {v}"
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("pid", type=int)
    ap.add_argument("--hex", action="append")
    ap.add_argument("--text", action="append")
    ap.add_argument("--guid", action="append")
    ap.add_argument("--u64", action="append")
    ap.add_argument("--context", type=int, default=48)
    ap.add_argument("--max", type=int, default=20, help="hits shown per pattern")
    a = ap.parse_args()
    pats = patterns(a)
    longest = max(len(p) for p in pats)
    shown: dict[str, int] = {}
    mem = open(f"/proc/{a.pid}/mem", "rb", 0)
    for start, end, name in regions(a.pid):
        off = start
        while off < end:
            n = min(CHUNK + longest, end - off)
            try:
                mem.seek(off)
                buf = mem.read(n)
            except (OSError, ValueError):
                break
            if not buf:
                break
            for p, label in pats.items():
                i = buf.find(p)
                while i >= 0 and i < CHUNK:
                    shown[label] = shown.get(label, 0) + 1
                    if shown[label] <= a.max:
                        lo = max(0, i - a.context)
                        print(f"{off + i:#014x}  {name}  {label}")
                        print(f"    -{i - lo:3d}: {buf[lo:i].hex(' ', 8)}")
                        print(f"    +  0: {buf[i:i + len(p) + a.context].hex(' ', 8)}")
                    i = buf.find(p, i + 1)
            off += CHUNK
    for label in sorted({v for v in pats.values()}):
        print(f"{shown.get(label, 0):6d}  {label}")


if __name__ == "__main__":
    main()
