#!/usr/bin/env python3
"""Where the running game keeps placed world objects: for objects near a point (or given by id), their exact world
matrix from the map's cells (tools/forge/world.py), searched for in the game's memory. Read-only on the game.

Each hit is the object's position (3 float32, exactly as in the cell) somewhere in memory; the report says what
surrounds it: the whole 4x4 matrix as in the cell (row-major, translation in the last row), the rotation rows
before it, or a bare position. This is the groundwork for live edits (moving/removing an object in the running
game): the copies that make the object appear and collide are among these.

  findobj.py <pid> <map> --near X,Y[,R] [--count N]     the N objects nearest to (X, Y) within R m (default 60)
  findobj.py <pid> <map> --id 0x...                      one object
<map>: Alpes, Japan, Alaska, WhiteRoom (the index of mods/world_editor is used to find the cells:
~/.cache/powder/world_editor/<map>.tsv, made by `world.py index`).
"""

import argparse
import math
import os
import re
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "forge"))
sys.path.insert(0, HERE)
import world  # noqa: E402
from storemap import chunks  # noqa: E402

STEEP = os.path.expanduser("~/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/"
                           "Steep")
INDEX = os.path.expanduser("~/.cache/powder/world_editor")


def index_rows(map_name: str):
    for line in open(os.path.join(INDEX, f"{map_name}.tsv")):
        p = line.rstrip("\n").split("\t")
        if p[0] == "obj":
            yield {"layer": int(p[1]), "cell": int(p[2]), "id": int(p[3], 16), "kind": p[4],
                   "pos": tuple(map(float, p[5:8])), "name": p[13]}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("pid", type=int)
    ap.add_argument("map")
    ap.add_argument("--near")
    ap.add_argument("--id")
    ap.add_argument("--count", type=int, default=6)
    ap.add_argument("--steep", default=os.environ.get("POWDER_STEEP_DIR") or STEEP)
    a = ap.parse_args()
    rows = list(index_rows(a.map))
    if a.id:
        want = [r for r in rows if r["id"] == int(a.id, 16)]
    else:
        x, y, *rad = map(float, a.near.split(","))
        r0 = rad[0] if rad else 60.0
        near = sorted(((math.dist((x, y), r["pos"][:2]), r) for r in rows if r["kind"] in ("entity", "group")),
                      key=lambda dr: dr[0])
        want = [r for d, r in near if d <= r0][:a.count]
    if not want:
        sys.exit("no object there")
    mp = world.Map(os.path.join(a.steep, f"DataPC_STP_{a.map}.forge"))
    cells, objs = {}, []
    for r in want:
        key = (r["cell"], r["layer"])
        if key not in cells:
            cells[key] = world.load_cell(mp, r["cell"], r["layer"], None)
        pl = next((p for p in cells[key].objects() if p.res.id == r["id"]), None)
        if pl is None or pl.matrix is None:
            continue
        m = pl.matrix
        objs.append((r, m, struct.pack("<3f", *m[12:15]), struct.pack("<12f", *m[0:12])))
        print(f"{r['name']} ({r['id']:#x}, cell {r['cell']}): pos {m[12]:.3f} {m[13]:.3f} {m[14]:.3f}")
    pat = re.compile(b"|".join(re.escape(o[2]) for o in objs))
    mem = open(f"/proc/{a.pid}/mem", "rb", 0)
    hits = 0
    for off, buf in chunks(mem, a.pid, writable=True):
        for mt in pat.finditer(buf):
            i = mt.start()
            o = next(o for o in objs if o[2] == mt.group())
            r, m, pos, rot = o
            before = buf[max(0, i - 48):i]
            if before == rot:
                kind = "4x4 as in the cell (rotation rows before)"
            elif buf[max(0, i - 64):i - 16] == rot or buf[max(0, i - 52):i - 4] == rot:
                kind = "rotation rows before (padded)"
            else:
                kind = "bare position"
            ctx = struct.unpack_from("<8f", buf, max(0, i - 16)) if i >= 16 and i + 16 <= len(buf) else ()
            print(f"  {off + i:#014x} {r['name'][:40]:40} {kind}; around: "
                  + " ".join(f"{v:.3g}" for v in ctx))
            hits += 1
    print(f"{hits} hit(s)")


if __name__ == "__main__":
    main()
