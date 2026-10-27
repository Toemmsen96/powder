#!/usr/bin/env python3
"""Build the named content mods together (native/sdk/CONTENT_MODS.md): every `mods/<name>/content.toml`'s additions
merged into one set of `<Base>/` entry folders for the session's `_patch_03` archives, plus the server's item list,
store offers and grants. Two mods that add costumes can't both ship Game Bootstrap Settings or the character
templates (the newest copy of an entry wins); here those are patched once with every mod's costumes.

  compose.py --game <Steep> --data <server data dir> --mods a,b [--mods-dir mods] -o <out> [--cache DIR] [--force]

content.toml (data only; paths relative to the mod's folder and inside it):

  [[costume]]
  name   = "COSTUME_MF_POWDER_MASTER_CHIEF"     # unique; every id derives from it
  like   = "COSTUME_MF_URBAN_OUTFIT_JAPAN_139"  # or costume = "STP_M_Costume_016"
  label  = "PowderChief"                         # optional: names of the cloned resources
  source = "assets/model"                        # optional: entries to clone from (the mod's model)
  title  = "Master Chief"                        # optional: its name in every language
  icon   = "assets/icon.png"                     # optional: its picture in the costume list
  store  = "COSTUME_M_MILITARY_GREY_94"          # optional: the item-list entry it copies (where it is listed)
  grant  = true                                  # optional: every player owns it (else a free store offer)

  [[world]]
  map   = "Alpes"                                # Alpes, Japan, Alaska, WhiteRoom
  edits = "assets/park.json"                     # the world editor's edit list format (world.py apply)

`<out>/<Base>/` gets one folder per archive base (symlinks into the cache). The cache key covers the tools, the
game's archives (names, sizes, times) and the mods' files, so an unchanged set is built once. The item list needs
the game's own list once in `<data>/game_item_list.json` (tools/trainer/storemap.py --item-list); without it the
costumes are built but not listed or owned.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "trainer"))
import items  # noqa: E402

MAPS = {"Alpes", "Japan", "Alaska", "WhiteRoom"}
DEFAULT_STORE = "COSTUME_M_MILITARY_GREY_94"  # a plain store costume: listed with the costumes, no unlock token
LIMITS = {"content.toml": 64 << 10, "icon": 8 << 20, "edits": 16 << 20}
TOOLS = ("compose.py", "items.py", "loc.py", "texture.py", "world.py", "forge.py", "../trainer/itemlist.py")


class ModError(Exception):
    pass


def inside(mod_dir: str, rel: str, what: str, limit: int | None = None) -> str:
    """A path from a mod's content.toml: relative, inside the mod's folder, existing, not too big."""
    if not isinstance(rel, str) or os.path.isabs(rel):
        raise ModError(f"{what}: a relative path inside the mod")
    path = os.path.realpath(os.path.join(mod_dir, rel))
    if not path.startswith(os.path.realpath(mod_dir) + os.sep):
        raise ModError(f"{what}: {rel} leaves the mod's folder")
    if not os.path.exists(path):
        raise ModError(f"{what}: {rel} does not exist")
    if limit and os.path.isfile(path) and os.path.getsize(path) > limit:
        raise ModError(f"{what}: {rel} is larger than {limit >> 20} MiB")
    return path


def read_mod(mods_dir: str, name: str) -> dict | None:
    """A mod's declared additions, checked; None for a mod without content.toml."""
    mod_dir = os.path.join(mods_dir, name)
    path = os.path.join(mod_dir, "content.toml")
    if not os.path.isfile(path):
        return None
    if os.path.getsize(path) > LIMITS["content.toml"]:
        raise ModError(f"{name}: content.toml is too large")
    with open(path, "rb") as f:
        data = tomllib.load(f)
    unknown = set(data) - {"costume", "world"}
    if unknown:
        raise ModError(f"{name}: content.toml: unknown tables {sorted(unknown)}")
    costumes, worlds = [], []
    for k, t in enumerate(data.get("costume", [])):
        what = f"{name}: costume #{k + 1}"
        cname = t.get("name", "")
        if not re.fullmatch(r"[A-Za-z0-9_]{4,100}", cname):
            raise ModError(f"{what}: name must be 4-100 letters, digits, underscores")
        if bool(t.get("like")) == bool(t.get("costume")):
            raise ModError(f"{what}: give exactly one of like / costume")
        if set(t) - {"name", "like", "costume", "label", "source", "title", "icon", "store", "grant"}:
            raise ModError(f"{what}: unknown keys {sorted(set(t) - {'name', 'like', 'costume', 'label', 'source', 'title', 'icon', 'store', 'grant'})}")
        title = t.get("title")
        if title is not None and (not isinstance(title, str) or not 0 < len(title) <= 60):
            raise ModError(f"{what}: title is 1-60 characters")
        srcs = t.get("source", [])
        srcs = [srcs] if isinstance(srcs, str) else list(srcs)
        spec = items.CostumeSpec(name=cname, like=t.get("like"), costume=t.get("costume"), label=t.get("label"),
                                 source_dirs=[inside(mod_dir, s, f"{what}: source") for s in srcs], title=title,
                                 icon=inside(mod_dir, t["icon"], f"{what}: icon", LIMITS["icon"]) if t.get("icon")
                                 else None)
        costumes.append({"spec": spec, "store": t.get("store"), "grant": bool(t.get("grant", False)), "mod": name})
    for k, t in enumerate(data.get("world", [])):
        what = f"{name}: world #{k + 1}"
        if t.get("map") not in MAPS:
            raise ModError(f"{what}: map is one of {sorted(MAPS)}")
        worlds.append({"map": t["map"], "edits": inside(mod_dir, t.get("edits", ""), f"{what}: edits",
                                                       LIMITS["edits"]), "mod": name})
    return {"name": name, "dir": mod_dir, "costumes": costumes, "worlds": worlds}


def fingerprint(game: str, mods: list[dict]) -> str:
    """The cache key: the tools' code, the game's archives (name, size, time) and every file of the mods."""
    h = hashlib.sha256()
    for t in TOOLS:
        with open(os.path.join(HERE, t), "rb") as f:
            h.update(f.read())
    for n in sorted(os.listdir(game)):
        if n.endswith(".forge") and "_patch_03" not in n:
            st = os.stat(os.path.join(game, n))
            h.update(f"{n}:{st.st_size}:{int(st.st_mtime)}".encode())
    for m in mods:
        h.update(m["name"].encode())
        for root, _, files in sorted(os.walk(m["dir"])):
            if os.sep + "client" + os.sep in root + os.sep:
                continue  # generated output of old-style asset mods: not an input
            for f in sorted(files):
                p = os.path.join(root, f)
                st = os.stat(p)
                h.update(f"{os.path.relpath(p, m['dir'])}:{st.st_size}:{int(st.st_mtime)}".encode())
    return h.hexdigest()[:24]


def build(game: str, mods: list[dict], dest: str) -> dict:
    """Everything the mods add, into dest/<Base>/; returns the summary (costumes with their ids)."""
    tmp = dest + ".tmp"
    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp)
    costumes = [c for m in mods for c in m["costumes"]]
    summary = {"costumes": [], "worlds": []}
    if costumes:
        plans = items.add_costumes(items.Game(game), [c["spec"] for c in costumes], os.path.join(tmp, "costumes"))
        for c, p in zip(costumes, plans):
            summary["costumes"].append({"name": c["spec"].name, "mod": c["mod"], "item": f"{p.item.id:#x}",
                                        "store": c["store"], "like": p.report["like"], "grant": c["grant"]})
        for base in os.listdir(os.path.join(tmp, "costumes")):
            if os.path.isdir(os.path.join(tmp, "costumes", base)):
                os.rename(os.path.join(tmp, "costumes", base), os.path.join(tmp, base))
    by_map: dict[str, list] = {}
    for w in (w for m in mods for w in m["worlds"]):
        with open(w["edits"]) as f:
            d = json.load(f)
        by_map.setdefault(w["map"], []).extend(d.get("edits", []) if isinstance(d, dict) else d)
        summary["worlds"].append({"mod": w["mod"], "map": w["map"]})
    for mp, edits in by_map.items():
        merged = os.path.join(tmp, f"{mp}.edits.json")
        with open(merged, "w") as f:
            json.dump({"edits": edits}, f)
        base = f"DataPC_STP_{mp}"
        r = subprocess.run([sys.executable, os.path.join(HERE, "world.py"), "apply", os.path.join(game, base + ".forge"),
                            merged, "-o", os.path.join(tmp, base)], capture_output=True, text=True)
        if r.returncode:
            raise ModError(f"world edits for {mp}: {r.stderr.strip() or r.stdout.strip()}")
    with open(os.path.join(tmp, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    shutil.rmtree(dest, ignore_errors=True)
    os.rename(tmp, dest)
    return summary


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--game", required=True, help="the Steep folder")
    ap.add_argument("--data", required=True, help="the server's data folder (item list, store, grants)")
    ap.add_argument("--mods", default="", help="mod names, comma-separated, in order")
    ap.add_argument("--mods-dir", default=os.path.join(HERE, "..", "..", "mods"))
    ap.add_argument("-o", "--out", required=True, help="folder for the session: <out>/<Base>/ (symlinks)")
    ap.add_argument("--cache", default=os.path.join(os.environ.get("XDG_CACHE_HOME") or
                                                    os.path.expanduser("~/.cache"), "powder", "compose"))
    ap.add_argument("--force", action="store_true", help="build even if the cache has this set")
    a = ap.parse_args(argv)
    try:
        mods = [m for m in (read_mod(a.mods_dir, n) for n in a.mods.split(",") if n.strip()) if m]
        seen: dict[str, str] = {}
        for m in mods:
            for c in m["costumes"]:
                if c["spec"].name in seen:
                    raise ModError(f"{c['spec'].name}: added by both {seen[c['spec'].name]} and {m['name']}")
                seen[c["spec"].name] = m["name"]
        key = fingerprint(a.game, mods)
        dest = os.path.join(a.cache, key)
        if mods and (a.force or not os.path.isfile(os.path.join(dest, "summary.json"))):
            print(f"compose: building {', '.join(m['name'] for m in mods)} ({key})")
            summary = build(a.game, mods, dest)
        elif mods:
            with open(os.path.join(dest, "summary.json")) as f:
                summary = json.load(f)
            print(f"compose: {', '.join(m['name'] for m in mods)} unchanged, from the cache ({key})")
        else:
            summary = {"costumes": [], "worlds": []}
    except (ModError, SystemExit) as e:
        print(f"compose: {e}", file=sys.stderr)
        return 1
    os.makedirs(a.out, exist_ok=True)
    for base in sorted(os.listdir(dest)) if mods else ():
        if os.path.isdir(os.path.join(dest, base)) and base.startswith("DataPC"):
            link = os.path.join(a.out, base)
            if os.path.lexists(link):
                raise SystemExit(f"compose: {link} exists")
            os.symlink(os.path.join(dest, base), link)
    game_list = os.path.join(a.data, "game_item_list.json")
    if os.path.isfile(game_list):
        import itemlist
        with open(game_list) as f:
            listed = {it["nameId"] for it in json.load(f)["items"]}
        # where it is listed: its own choice, else its source's entry, else a plain store costume's
        store = {c["name"]: c["store"] or (c["like"] if c["like"] in listed else DEFAULT_STORE)
                 for c in summary["costumes"]}
        itemlist.write(game_list, a.data, [(c["name"], c["item"], store[c["name"]]) for c in summary["costumes"]],
                       grants=[c["name"] for c in summary["costumes"] if c["grant"]])
    elif summary["costumes"]:
        print(f"compose: no {game_list}: the costumes are built but not listed or owned "
              "(tools/trainer/storemap.py <pid> <Steep> --item-list <it>, with the game running)", file=sys.stderr)
    for c in summary["costumes"]:
        print(f"  costume {c['name']} ({c['mod']}): item {c['item']}" + (", granted" if c["grant"] else ""))
    for w in summary["worlds"]:
        print(f"  world edits: {w['mod']} on {w['map']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
