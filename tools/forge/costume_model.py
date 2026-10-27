#!/usr/bin/env python3
"""A costume's own model and textures put into the game's costume entries, from the player's install: the generic half
of mods/master_chief_costume/make_chief.py, so a content mod ships only its own files (native/sdk/CONTENT_MODS.md).

Inputs: per gender a glTF (.glb) already fitted to Steep's skeleton - bone names are Steep's, one primitive per Steep
material of the costume (its name is the material's), as `mesh.py export --rig` / make_chief.py --export-assets write
it - and per Steep material its maps (diffuse, normal, specular, mask: PNGs, row 0 at v = 0 as the game samples UV
textures). Output: the costume's entries with those, under their own ids (an unpacked folder per entry), for
`items.py add-costume(s)` to clone from (`source`).

  costume_model.py <Steep> STP_M_Costume_016 --model-m chief_M.glb --model-f chief_F.glb \\
      --texture STP_MF_Costume_016_Jacket_ALL:diffuse=armour_d.png ... -o <out>
"""
from __future__ import annotations

import argparse
import os
import re
import struct
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import forge  # noqa: E402
import mesh  # noqa: E402

BASE = "DataPC_extra_chr"
SLOTS = {0: "diffuse", 1: "normal", 2: "specular", 7: "mask"}  # TextureSet slot -> map
MATERIAL_ENTRY, TEXTURE_SET = 0x85C817C3, 0xD70E6670


def run(*cmd: str) -> str:
    p = subprocess.run([sys.executable, *cmd], capture_output=True, text=True)
    if p.returncode:
        raise RuntimeError(f"{os.path.basename(cmd[0])} {cmd[1] if len(cmd) > 1 else ''}: {p.stderr or p.stdout}")
    return p.stdout


def holders(game: str, ids: list[int]) -> set[tuple[str, str]]:
    """(base archive, entry id) of every datafile that holds one of these resources."""
    out = run(os.path.join(HERE, "forge.py"), "where", game, *[x for i in ids for x in ("--id", f"{i:#x}")])
    return {(m[2], m[1]) for m in re.finditer(r"entry (0x[0-9a-f]+) .*\(base (\S+)\)", out)}


def texture_sets(game: str, costume_m: str, materials: list[str], embedded: bool = True) -> dict[str, list[dict]]:
    """Steep material -> [{map: TextureMap id}] of every texture set it shows in: the one embedded in the costume's
    entity (unless `embedded` is False: some are a shared placeholder) and its colourways, the material entries
    `<material without _ALL>_T01..` the costume's items pick from."""
    arch = os.path.join(game, BASE + ".forge")
    s = mesh.load_source(arch, costume_m)
    by_name = {r.name: r for r in s.resources}

    def slots(ts):
        return {SLOTS[k]: struct.unpack_from("<Q", ts.payload, 13 + 10 * k + 2)[0]
                for k in SLOTS if ts.payload[13 + 10 * k] == 1}
    fg = forge.Forge(arch)
    out = {}
    for mat in materials:
        sets = []
        if embedded and mat in by_name:
            payload = by_name[mat].payload
            ts = next((r for r in s.resources if r.type == TEXTURE_SET and struct.pack("<Q", r.id) in payload), None)
            if ts is not None:
                sets.append(slots(ts))
        pattern = re.escape(mat.removesuffix("_ALL")) + r"_T\d+"
        for e in fg.entries:
            if e.type == MATERIAL_ENTRY and re.fullmatch(pattern, e.name):
                newest, entry = mesh.game_entry(arch, f"{e.id:#x}")
                for r in forge.parse_datafile(newest.read(entry)).resources:
                    if r.type == TEXTURE_SET and slots(r) not in sets:
                        sets.append(slots(r))
        if not sets:
            raise RuntimeError(f"{mat}: no texture set (neither in {costume_m} nor a {mat.removesuffix('_ALL')}_T##)")
        out[mat] = sets
    return out


def build(game: str, costume_m: str, models: dict[str, str], textures: dict[str, dict[str, str]], out: str,
          embedded: bool = True) -> None:
    """The costume's entries (both genders' entities in every LOD, the texture sets' maps) with the mod's model and
    textures, into out/<Base>/ (unpacked entries under the game's ids)."""
    arch = os.path.join(game, BASE + ".forge")
    for g, glb in models.items():
        costume = costume_m if g == "M" else costume_m.replace("STP_M_", "STP_F_", 1)
        lods = [r.id for _, r in mesh.mesh_resources(mesh.load_source(arch, costume), None)]
        for base, entry in sorted(holders(game, lods)):
            run(os.path.join(HERE, "mesh.py"), "import", os.path.join(game, base + ".forge"), entry, "--glb", glb,
                "--to", "LOD", "--ignore-names", "-o", os.path.join(out, base))
    sets = texture_sets(game, costume_m, list(textures), embedded)
    where = holders(game, [t for lst in sets.values() for m in lst for t in m.values()])
    for mat, lst in sets.items():
        for ids in lst:
            for k, tid in ids.items():
                if k not in textures[mat]:
                    continue
                for base in sorted({b for b, e in where if int(e, 16) == tid}):
                    run(os.path.join(HERE, "texture.py"), "replace", os.path.join(game, base + ".forge"), f"{tid:#x}",
                        "--image", textures[mat][k], "--raw-rows", "-o", os.path.join(out, base))


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("game")
    ap.add_argument("costume", help="the male costume selector/entity, e.g. STP_M_Costume_016 (the female follows)")
    ap.add_argument("--model-m", required=True)
    ap.add_argument("--model-f")
    ap.add_argument("--texture", action="append", default=[], metavar="MATERIAL:MAP=PNG",
                    help="e.g. STP_MF_Costume_016_Jacket_ALL:diffuse=armour_d.png (maps: diffuse normal specular mask)")
    ap.add_argument("--no-embedded", action="store_true", help="leave the entity's embedded texture set alone")
    ap.add_argument("-o", "--out", required=True)
    a = ap.parse_args(argv)
    textures: dict[str, dict[str, str]] = {}
    for t in a.texture:
        m = re.fullmatch(r"([^:]+):(diffuse|normal|specular|mask)=(.+)", t)
        if not m:
            sys.exit(f"--texture {t}: MATERIAL:MAP=PNG")
        textures.setdefault(m[1], {})[m[2]] = m[3]
    models = {"M": a.model_m, **({"F": a.model_f} if a.model_f else {})}
    build(a.game, a.costume, models, textures, a.out, not a.no_embedded)
    print(f"{a.costume}: {len(models)} model(s), {sum(len(v) for v in textures.values())} map(s) -> {a.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
