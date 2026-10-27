#!/usr/bin/env python3
"""World editor, live edits: the session helper. For the session's lifetime it watches the server's edit list and
turns every change into complete cell datafiles that the client DLL (src/live.cpp) serves in place of the
archives' bytes, so an edit shows the next time the game streams its cell in (README.md "Live edits").

    live.py run                  what session.sh starts in the background (settings from the environment, as for
                                 apply.py session: POWDER_STEEP_DIR, POWDER_DATA_DIR, POWDER_HTTPS_PORT,
                                 POWDER_MOD_RUNTIME; WORLD_EDITOR_ADDS=0)
    live.py once --base EDITS --edits EDITS [--steep DIR] --out DIR
                                 one generation by hand: what the game would be served after the session started
                                 with the list BASE and the server now holds EDITS (files into DIR)

Per changed map it runs `world.py apply` (as the session step does), compares each cell it writes, and each cell
it wrote before, with what the game reads for that entry (the newest copy in the archive stack, this session's
_patch_03 included), and writes the cells that differ into <runtime>/live/: `<id>-<hash>.bin` (the whole datafile)
and manifest.tsv (written last, by rename), one line per served cell:

    cell  archive  entry-index  off,id,size  prev-record  next-record  same|grow  file  bytes  map  cell-name

"same": the datafile is fitted to exactly the entry's size (forge.py fit_datafile: recompressed, then padded with
empty zstd blocks), served over the entry's own bytes. "grow": it does not fit; the DLL points the entry's index
record at a slot past the end of the archive (prev/next records let it find and check the record in memory).

The helper stops when the runtime folder goes (powder-wrap removes it with the session) or is made anew, or when
the server has not answered for 3 minutes. Game data is only read. Python 3.10+; forge.py's zstd module.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "tools", "forge"))
sys.path.insert(0, HERE)
import apply  # noqa: E402  (this folder: the session step's fetch and generate)
import forge  # noqa: E402

MAPS = apply.MAPS
POLL = 0.5           # seconds between looks at the panel's poke file
ASK_EVERY = 60       # seconds between asks for the edit list without a poke (edits made elsewhere)
GIVE_UP = 180        # seconds without the server: the session is over
MANIFEST_VERSION = 1


def log(msg: str) -> None:
    print(f"world_editor live: {msg}", file=sys.stderr, flush=True)


def edits_of(edits: list[dict], m: str) -> list[dict]:
    return [e for e in edits if isinstance(e, dict) and apply.map_of(e) == m]


def signature(edits: list[dict]) -> str:
    return json.dumps(edits, sort_keys=True)


def same_resources(a: list[forge.Resource], b: list[forge.Resource]) -> bool:
    key = lambda r: (r.id, r.type, r.name, r.payload, r.flag, r.pre, tuple(r.extra))  # noqa: E731
    return len(a) == len(b) and all(key(x) == key(y) for x, y in zip(a, b))


class Stack:
    """A map's archives as the game sees them (session archives included) and as shipped (without them)."""

    def __init__(self, steep: str, m: str):
        base = os.path.join(steep, f"DataPC_STP_{m}.forge")
        skip = set()
        try:
            with open(os.path.join(steep, ".powder-assets")) as f:
                skip = {line.strip() for line in f if line.strip()}
        except OSError:
            pass
        self.game: dict[int, tuple[forge.Forge, forge.Entry]] = {}
        self.shipped: dict[int, tuple[forge.Forge, forge.Entry]] = {}
        for p in forge.patch_stack(base):
            if not os.path.exists(p):
                continue
            fg = forge.Forge(p)
            size = os.path.getsize(p)
            for e in fg.entries:
                if e.id in (forge.META_ID, forge.PREFETCH_ID) or e.offset + e.size > size:
                    continue
                self.game[e.id] = (fg, e)
                if os.path.basename(p) not in skip:
                    self.shipped[e.id] = (fg, e)

    @staticmethod
    def record(fg: forge.Forge, index: int) -> str:
        if not 0 <= index < len(fg.entries):
            return "-"
        e = fg.entries[index]
        return f"{e.offset},{e.id:#x},{e.size}"


class Live:
    def __init__(self, steep: str, out: str):
        self.steep, self.out = steep, out
        self.sig: dict[str, str] = {}             # map -> the edit list its cells were made from
        self.ever: dict[str, set[int]] = {}       # map -> every cell entry any list so far changed
        self.lines: dict[int, str] = {}           # entry id -> its manifest line (served cells)
        self.files: dict[int, str] = {}           # entry id -> its data file
        self.gen = 0
        self.newest = 0
        self.base: list[dict] = []
        os.makedirs(out, exist_ok=True)

    def cells(self, m: str, edits: list[dict]) -> dict[int, list[forge.Resource]]:
        """The cells `edits` changes on map m (world.py apply, via the session step's generate)."""
        tmp = tempfile.mkdtemp(prefix="world_editor-live-")
        try:
            if edits:
                apply.generate(edits, self.steep, tmp)
            out = {}
            folder = os.path.join(tmp, f"DataPC_STP_{m}")
            for d in sorted(os.listdir(folder)) if os.path.isdir(folder) else []:
                meta, res = forge.load_unpacked(os.path.join(folder, d))
                out[int(meta["entry"], 16)] = res
            return out
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def start(self, base: list[dict]) -> None:
        """The session started with `base` (its cells are what the game already reads)."""
        self.base = base
        for m in MAPS:
            self.sig[m] = signature(edits_of(base, m))
        self.write_manifest()

    def update(self, edits: list[dict]) -> bool:
        changed = [m for m in MAPS if signature(edits_of(edits, m)) != self.sig.get(m)]
        self.newest = max([e.get("n", 0) for e in edits if isinstance(e.get("n"), int)] or [0])
        if not changed:
            return False
        for m in changed:
            t = time.time()
            if m not in self.ever:   # the cells the session's own list changed (in its _patch_03): once per map
                self.ever[m] = set(self.cells(m, edits_of(self.base, m)))
            want = self.cells(m, edits_of(edits, m))
            self.ever[m] |= set(want)
            st = Stack(self.steep, m)
            served = same = grow = 0
            for eid in sorted(self.ever[m]):
                if eid not in st.game:
                    continue
                gfg, ge = st.game[eid]
                res = want.get(eid)
                if res is None:                       # no edit left in it: the cell as shipped
                    if eid not in st.shipped:
                        continue
                    sfg, se = st.shipped[eid]
                    res = forge.parse_datafile(sfg.read(se)).resources
                if same_resources(res, forge.parse_datafile(gfg.read(ge)).resources):
                    self.drop(eid)
                    continue
                data, fits = forge.fit_datafile(res, ge.size)
                forge.parse_datafile(data)             # self-check
                name = f"{eid:016x}-{hashlib.sha1(data).hexdigest()[:12]}.bin"
                path = os.path.join(self.out, name)
                if not os.path.exists(path):
                    with open(path + ".tmp", "wb") as f:
                        f.write(data)
                    os.replace(path + ".tmp", path)
                self.files[eid] = name
                self.lines[eid] = "\t".join([
                    "cell", os.path.basename(gfg.path), str(ge.index), Stack.record(gfg, ge.index),
                    Stack.record(gfg, ge.index - 1), Stack.record(gfg, ge.index + 1), "same" if fits else "grow",
                    name, str(len(data)), m, ge.name])
                served += 1
                same += fits
                grow += not fits
            self.sig[m] = signature(edits_of(edits, m))
            log(f"{m}: {served} cell(s) served ({same} same size, {grow} bigger: index redirected) in "
                f"{time.time() - t:.1f} s")
        self.gen += 1
        self.write_manifest()
        return True

    def drop(self, eid: int) -> None:
        self.lines.pop(eid, None)
        self.files.pop(eid, None)

    def write_manifest(self) -> None:
        text = (f"# powder world_editor live v{MANIFEST_VERSION}: mods/world_editor/live.py (src/live.cpp reads it)\n"
                f"gen\t{self.gen}\tedits\t{self.newest}\ttime\t{int(time.time())}\n"
                + "".join(self.lines[k] + "\n" for k in sorted(self.lines)))
        path = os.path.join(self.out, "manifest.tsv")
        with open(path + ".tmp", "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        os.replace(path + ".tmp", path)
        keep = set(self.files.values()) | {"manifest.tsv", "session_edits.json"}
        cutoff = time.time() - 60                    # the DLL has read anything older than that
        for n in os.listdir(self.out):
            p = os.path.join(self.out, n)
            if n not in keep and n.endswith(".bin") and os.path.getmtime(p) < cutoff:
                os.remove(p)


def same_folder(path: str, ino: int) -> bool:
    try:
        return os.stat(path).st_ino == ino
    except OSError:
        return False


def cmd_run(a) -> int:
    runtime = os.environ.get("POWDER_MOD_RUNTIME")
    if not runtime or not os.path.isdir(runtime):
        log("no POWDER_MOD_RUNTIME (no overlay this session): live edits off")
        return 0
    steep = os.environ.get("POWDER_STEEP_DIR") or apply.DEFAULT_STEEP
    data_dir = os.environ.get("POWDER_DATA_DIR")
    port = int(os.environ.get("POWDER_HTTPS_PORT") or 443)
    cafile = os.path.join(data_dir, "tls", "ca.pem") if data_dir else None
    out = os.path.join(runtime, "live")
    ino = os.stat(runtime).st_ino
    started = time.time()
    lv = Live(steep, out)
    base_file = os.path.join(out, "session_edits.json")   # the list the session step used (apply.py writes it)
    try:
        with open(base_file, encoding="utf-8") as f:
            base = json.load(f).get("edits", [])
    except (OSError, ValueError):
        base = []
    lv.start(base)
    log(f"watching the server's edits (session started with {len(base)}); pid {os.getpid()}")
    poke = os.path.join(out, "poke")   # the panel touches it when the server took an edit
    last_ok = last_ask = 0.0
    seen = None
    while same_folder(runtime, ino):
        try:
            mark = os.stat(poke).st_mtime_ns
        except OSError:
            mark = None
        if mark == seen and time.time() - last_ask < ASK_EVERY:
            time.sleep(POLL)
            continue
        seen, last_ask = mark, time.time()
        try:
            edits = apply.fetch(port, cafile)["edits"] if cafile else []
            last_ok = time.time()
            lv.update(edits)
        except (OSError, ValueError) as ex:
            if time.time() - (last_ok or started) > GIVE_UP:
                log(f"the server has not answered for {GIVE_UP} s ({ex}): stopping")
                return 0
            last_ask = 0.0                             # try again soon
            time.sleep(2)
        except Exception as ex:  # a bad cell or list: report, keep watching
            log(f"error: {ex!r}")
            time.sleep(5)
        time.sleep(POLL)
    log("the session's runtime folder is gone: stopping")
    return 0


def cmd_once(a) -> int:
    def load(p):
        with open(p, encoding="utf-8") as f:
            d = json.load(f)
        return d.get("edits", []) if isinstance(d, dict) else d
    lv = Live(a.steep, a.out)
    lv.start(load(a.base) if a.base else [])
    lv.update(load(a.edits))
    print(open(os.path.join(a.out, "manifest.tsv"), encoding="utf-8").read(), end="")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)
    p = sp.add_parser("run", help="the session helper (settings from the environment)")
    p.set_defaults(fn=cmd_run)
    p = sp.add_parser("once", help="one generation by hand")
    p.add_argument("--base", help="the edit list the session started with (default: none)")
    p.add_argument("--edits", required=True, help="the edit list now")
    p.add_argument("--steep", default=os.environ.get("POWDER_STEEP_DIR") or apply.DEFAULT_STEEP)
    p.add_argument("--out", required=True)
    p.set_defaults(fn=cmd_once)
    a = ap.parse_args()
    return a.fn(a)


if __name__ == "__main__":
    sys.exit(main())
