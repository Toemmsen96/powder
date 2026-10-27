#!/usr/bin/env python3
"""Tests of the live edits' outside-the-game half, on the WhiteRoom of a local Steep install (read only):

- forge.py pad_datafile / fit_datafile: datafiles made exactly an entry's size decompress to the same resources
  (also through libzstd frame by frame), for every padding from 0 to 300 bytes;
- live.py over a session: a scratch game folder (links to the real archives) gets a session _patch_03 built from a
  first edit list, as powder-wrap does; then edits are added, undone and cleared, and after each step every cell
  live.py serves is read back by live_patch_test (live_patch.h, the DLL's byte-range logic, reading the archive in
  the game's aligned chunks) and must parse to exactly the cell `world.py apply` makes for that list (or the
  shipped cell); cells it does not serve must already be what the game reads.

    python3 mods/world_editor/tests/test_live.py      (or pytest; POWDER_STEEP_DIR = the Steep folder)

Needs g++ for live_patch_test (skipped without), Python 3.10+, zstandard.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
MOD = os.path.dirname(HERE)
ROOT = os.path.dirname(os.path.dirname(MOD))
sys.path.insert(0, os.path.join(ROOT, "tools", "forge"))
sys.path.insert(0, MOD)
import forge  # noqa: E402
import live  # noqa: E402

STEEP = os.environ.get("POWDER_STEEP_DIR") or os.path.expanduser(
    "~/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep")
WR = "DataPC_STP_WhiteRoom"


def have_game() -> bool:
    return os.path.exists(os.path.join(STEEP, WR + ".forge"))


def skip(why: str):
    try:
        import pytest
        pytest.skip(why)
    except ImportError:
        print(f"skipped: {why}")
        raise SystemExit(0)


def edit(n, op, cell=None, oid=None, **kw):
    e = {"n": n, "map": "WhiteRoom", "op": op}
    if oid:
        e["obj"] = {"layer": 0, "cell": cell, "id": oid}
    e.update(kw)
    return e


def test_pad_datafile():
    if not have_game():
        skip("no Steep install")
    import zstandard
    for name in ("Cell00035_DataBlock", "Cell00036_DataBlock"):
        fg, e = forge.newest_entry(os.path.join(STEEP, WR + ".forge"), name)
        res = forge.parse_datafile(fg.read(e)).resources
        data = forge.build_datafile(res, level=19)
        for d in range(301):
            p = forge.pad_datafile(data, len(data) + d)
            if d in (1, 2, 5):
                assert p is None
                continue
            assert p is not None and len(p) == len(data) + d, (name, d)
            df = forge.parse_datafile(p)
            assert live.same_resources(df.resources, res)
            for b in (df.header_block, df.body_block):
                assert b.bad_checksums() == 0
                for c in b.chunks:
                    assert c.csize < c.usize
                    assert zstandard.ZstdDecompressor().decompress(c.data, max_output_size=c.usize) == \
                        forge.decompress(forge.CODEC_ZSTD, c.data, c.usize)
        assert forge.pad_datafile(data, len(data) - 1) is None
        fitted, ok = forge.fit_datafile(res, e.size)
        assert ok and len(fitted) == e.size and live.same_resources(forge.parse_datafile(fitted).resources, res)


def build_tool(tmp: str) -> str | None:
    if not shutil.which("g++"):
        return None
    exe = os.path.join(tmp, "live_patch_test")
    subprocess.run(["g++", "-std=c++17", "-O1", "-Wall", "-I", os.path.join(MOD, "src"),
                    os.path.join(HERE, "live_patch_test.cpp"), "-o", exe], check=True)
    return exe


def check_step(lv: live.Live, game: str, out: str, tool: str | None, edits: list[dict], label: str) -> dict:
    lv.update(edits)
    want = lv.cells("WhiteRoom", live.edits_of(edits, "WhiteRoom"))
    st = live.Stack(game, "WhiteRoom")
    lines = [ln.split("\t") for ln in open(os.path.join(out, "manifest.tsv")) if ln.startswith("cell\t")]
    served = {int(c[3].split(",")[1], 16): c for c in lines}
    back = os.path.join(os.path.dirname(out), "back")
    shutil.rmtree(back, ignore_errors=True)
    os.makedirs(back)
    if tool:
        r = subprocess.run([tool, game, out, back], capture_output=True, text=True)
        assert r.returncode == 0, r.stdout + r.stderr
    modes = {}
    for eid in sorted(set(want) | lv.ever["WhiteRoom"]):
        gfg, ge = st.game[eid]
        expect = want.get(eid)
        if expect is None:
            sfg, se = st.shipped[eid]
            expect = forge.parse_datafile(sfg.read(se)).resources
        game_res = forge.parse_datafile(gfg.read(ge)).resources
        if eid not in served:
            assert live.same_resources(expect, game_res), f"{label}: {ge.name} differs from the game's but is not served"
            continue
        c = served[eid]
        modes[ge.name] = c[6]
        assert c[1] == os.path.basename(gfg.path) and int(c[2]) == ge.index
        assert c[3] == f"{ge.offset},{ge.id:#x},{ge.size}"
        data = open(os.path.join(out, c[7]), "rb").read()
        assert len(data) == int(c[8]) and (c[6] != "same" or len(data) == ge.size)
        if tool:   # what the game reads, through the DLL's range logic
            got = open(os.path.join(back, f"{ge.id:#x}.bin"), "rb").read()
            assert got == data, f"{label}: {ge.name} read back differs"
        assert live.same_resources(forge.parse_datafile(data).resources, expect), f"{label}: {ge.name} wrong cell"
    for eid in served:
        assert eid in want or eid in lv.ever["WhiteRoom"]
    print(f"  {label}: {len(served)} cell(s) served: " + ", ".join(f"{k} ({v})" for k, v in sorted(modes.items())))
    return modes


WINE = os.environ.get("WORLD_EDITOR_WINE_TEST") == "1" or "--wine" in sys.argv


def winpath(p: str) -> str:
    return "Z:" + os.path.abspath(p).replace("/", "\\")


def wine_test(tmp: str, game: str, out: str, then) -> None:
    """world_editor.dll in livehost.exe (tests/livehost.cpp) under Wine, in a scratch prefix: every served cell read
    back through the DLL's hooks in each completion mode must be the served bytes; then `then()` writes the next
    generation, after which a grown cell no longer served reads as the archive has it (its record restored)."""
    tp = os.path.join(ROOT, "native", "third_party")
    dll = os.path.join(MOD, "client", "world_editor.dll")
    for need in (os.path.join(tp, "imgui"), os.path.join(tp, "minhook"), dll):
        assert os.path.exists(need), f"{need}: run native/build.sh mods first"
    cxx, cc = "x86_64-w64-mingw32-g++", "x86_64-w64-mingw32-gcc"
    objs = []
    for s in ("buffer", "hook", "trampoline"):
        o = os.path.join(tmp, f"mh_{s}.o")
        subprocess.run([cc, "-O1", "-c", os.path.join(tp, "minhook", "src", s + ".c"), "-o", o,
                        "-I", os.path.join(tp, "minhook", "include"), "-I", os.path.join(tp, "minhook", "src")],
                       check=True)
        objs.append(o)
    o = os.path.join(tmp, "hde64.o")
    subprocess.run([cc, "-O1", "-c", os.path.join(tp, "minhook", "src", "hde", "hde64.c"), "-o", o], check=True)
    objs.append(o)
    host = os.path.join(tmp, "livehost.exe")
    imgui = os.path.join(tp, "imgui")
    subprocess.run([cxx, "-O1", "-std=c++17", "-I", imgui, "-I", os.path.join(tp, "minhook", "include"),
                    "-I", os.path.join(ROOT, "native", "sdk"), os.path.join(HERE, "livehost.cpp")]
                   + [os.path.join(imgui, f + ".cpp") for f in ("imgui", "imgui_draw", "imgui_tables", "imgui_widgets")]
                   + objs + ["-o", host, "-static", "-static-libgcc", "-static-libstdc++"], check=True)
    shutil.copy(dll, os.path.join(game, "powder_mods", "world_editor.dll"))
    lines = [ln.rstrip("\n").split("\t") for ln in open(os.path.join(out, "manifest.tsv")) if ln.startswith("cell\t")]
    recs = os.path.join(tmp, "records.tsv")
    with open(recs, "w") as f:
        for a in sorted({c[1] for c in lines if c[6] == "grow"}):
            fg = forge.Forge(os.path.join(game, a))
            f.write(a + "".join(f"\t{e.offset},{e.id:#x},{e.size}" for e in fg.entries) + "\n")
    back = os.path.join(tmp, "wine-back")
    shutil.rmtree(back, ignore_errors=True)
    os.makedirs(back)
    env = dict(os.environ, WINEPREFIX=os.path.join(tmp, "prefix"), WINEDEBUG="-all",
               WINEDLLOVERRIDES="winedbg.exe=d")   # no crash dialog on the desktop
    proc = subprocess.Popen(["wine", host, winpath(os.path.join(game, "powder_mods", "world_editor.dll")),
                             winpath(game), winpath(recs), winpath(back)], env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True)
    seen = []
    for ln in proc.stdout:                          # phase 1: every mode
        seen.append(ln.rstrip())
        if "waiting for the next generation" in ln:
            break
    modes = sorted(os.listdir(back))
    assert len(modes) == 11 and "phase 1 ok" in seen[-1], "\n".join(seen[-40:])
    for mode in modes:
        for c in lines:
            got = open(os.path.join(back, mode, c[3].split(",")[1] + ".bin"), "rb").read()
            want = open(os.path.join(out, c[7]), "rb").read()
            assert got == want, f"wine, mode {mode}: {c[10]} read back differs"
    then()                                          # phase 2: the next generation
    open(os.path.join(back, "next"), "w").close()
    rest, _ = proc.communicate(timeout=300)
    seen += rest.splitlines()
    print("\n".join("    " + ln for ln in seen if "FAIL" in ln or "host:" in ln or "live:" in ln))
    assert proc.returncode == 0 and "host: ok" in seen[-1], "\n".join(seen[-40:])
    now = {c[3]: c for c in (ln.rstrip("\n").split("\t") for ln in open(os.path.join(out, "manifest.tsv"))
                             if ln.startswith("cell\t"))}
    for c in lines:
        got = open(os.path.join(back, "phase2", c[3].split(",")[1] + ".bin"), "rb").read()
        if c[3] in now:
            want = open(os.path.join(out, now[c[3]][7]), "rb").read()
        else:                                       # no longer served: the archive's own entry
            fg = forge.Forge(os.path.join(game, c[1]))
            want = fg.read(fg.entries[int(c[2])])
        assert got == want, f"wine, phase 2: {c[10]} read back differs"
    print(f"  wine: {len(lines)} cell(s) read back through world_editor.dll in {len(modes)} modes, then the next "
          f"generation ({len(now)} served, the grown cell's record restored): ok")


def test_live_session():
    if not have_game():
        skip("no Steep install")
    tmp = tempfile.mkdtemp(prefix="world_editor-test-")
    try:
        game = os.path.join(tmp, "game")
        os.makedirs(game)
        for p in forge.patch_stack(os.path.join(STEEP, WR + ".forge")):
            if os.path.exists(p) and "_patch_03" not in p:
                os.symlink(p, os.path.join(game, os.path.basename(p)))
        rock, rock27, chapel, fir = "0xd8d3d94fee", "0xd8d3d94143", "0xd8d3d932af", "0xd8d3d96665"
        base = [edit(1, "move", 35, rock, pos=[-10.0, 20.0, 3.0])]
        # the session start, as powder-wrap does it: the step's cells -> <Base>_patch_03.forge, listed as ours
        forge_dir = os.path.join(tmp, "forge")
        import apply
        assert apply.generate(base, game, forge_dir) == 0
        parts = [os.path.join(forge_dir, WR, d) for d in sorted(os.listdir(os.path.join(forge_dir, WR)))]
        subprocess.run([sys.executable, os.path.join(ROOT, "tools", "forge", "forge.py"), "build",
                        os.path.join(game, WR + "_patch_03.forge"), "--like", os.path.join(game, WR + ".forge")]
                       + parts, check=True, stdout=subprocess.DEVNULL)
        with open(os.path.join(game, ".powder-assets"), "w") as f:
            f.write(WR + "_patch_03.forge\n")
        out = os.path.join(game, "powder_mods", "world_editor", "live")   # where powder-wrap's runtime folder is
        tool = build_tool(tmp)
        if tool:
            r = subprocess.run([tool], capture_output=True, text=True)
            assert r.returncode == 0, r.stdout + r.stderr
        lv = live.Live(game, out)
        lv.start(base)
        assert [ln for ln in open(os.path.join(out, "manifest.tsv")) if ln.startswith("cell")] == []
        assert not lv.update(base)                     # the session's own list: nothing to do

        s1 = base + [edit(2, "turn", 27, rock27, deg=30.0), edit(3, "remove", 35, chapel)]
        m = check_step(lv, game, out, tool, s1, "turn + remove")
        assert m.get("Cell00035_DataBlock") == "same" and m.get("Cell00027_DataBlock") == "same"
        s2 = s1 + [edit(4, "add", name="Sapin", src={"layer": 0, "cell": 36, "id": fir}, pos=[2.3, 12.0, -0.55],
                        yaw=15.0, scale=1.0)]
        m = check_step(lv, game, out, tool, s2, "add a fir")
        assert m.get("Cell00036_DataBlock")            # (a copy of a fir of the same cell: it compresses away)
        s2 += [edit(7, "add", name="Sapin", src={"layer": 0, "cell": 36, "id": fir}, pos=[200.0, -300.0, 0.0])]
        m = check_step(lv, game, out, tool, s2, "add a fir to a small cell")
        assert m.get("Cell00013_DataBlock") == "grow"  # a small cell that gets one: needs the index redirected
        s3 = [e for e in s2 if e["n"] != 1]            # undo the session's own move: cell 35 from the shipped one
        check_step(lv, game, out, tool, s3, "undo #1")
        s4 = s3 + [edit(5, "move", 35, rock, pos=[-12.0, 22.0, 3.0]), edit(6, "scale", 35, rock, factor=1.5)]
        check_step(lv, game, out, tool, s4, "move + scale again")
        m = check_step(lv, game, out, tool, [], "clear")
        assert list(m) == ["Cell00035_DataBlock"]      # only the session's _patch_03 cell needs the shipped one back
        if WINE:   # the DLL in a stand-in host under Wine: grow + same cells, then all undone
            check_step(lv, game, out, tool, s2, "back to the grown state")
            wine_test(tmp, game, out, lambda: check_step(lv, game, out, tool, [], "clear (with the DLL running)"))
        print("live session: ok" + ("" if tool else " (no g++: read-back through live_patch.h skipped)"))
    finally:
        if os.environ.get("WORLD_EDITOR_TEST_KEEP") != "1":
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"kept {tmp}")


if __name__ == "__main__":
    test_pad_datafile()
    print("pad_datafile: ok")
    test_live_session()
