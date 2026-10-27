#!/usr/bin/env python3
"""Steep's gear items and the character template's selectors: list and describe them, and add a NEW costume.

Every gear item (jacket, costume, ski, ...) is a resource of type 0x3856B74B inside `Game Bootstrap Settings` (entry
0x800 of the DataPC stack). An item carries tags; the character template (`STP_MF_Template`, `STP_F_Template` and the
NPC templates) holds selectors (0x22ECBE63) whose branches are keyed by those tags and pick the mesh entity and the
materials the rider wears. `add-costume` clones a costume under new ids: a new item (appended to GEAR_COSTUMES, its
model tag registered), a new branch in STP_M_Clothing / STP_F_Clothing, new costume and texture selectors, new
entities, materials and textures. Nothing of the game's is replaced: the shipped entries the mod patches (the
bootstrap and the templates) only gain resources and list entries. The formats, confirmed vs guessed, are in
ITEMS.md next to this file.

  items.py list    <Steep folder> [-g NAME] [--category N]
  items.py show    <Steep folder> COSTUME_MF_URBAN_OUTFIT_JAPAN_139      # fields, tags, who references it, the chain
  items.py costumes <Steep folder>                                       # costume branches: tag -> selector -> entity
  items.py add-costume <Steep folder> --like COSTUME_MF_URBAN_OUTFIT_JAPAN_139 --name COSTUME_MF_POWDER_TEST_01 \
           -o mods/<mod>/client/forge [--source-dir DIR]... [--title "Name in the game"]

Archives are only read. `_patch_NN` archives past `_patch_02` are left out (they are asset mods' session archives,
not the game's). Needs Python 3.10+ and forge.py's zstd module.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import struct
import sys
import zlib
from dataclasses import dataclass, field

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forge  # noqa: E402
import loc  # noqa: E402

# resource types
ITEM, GEAR_LIST, TAG_REGISTRY, SELECTOR, TEMPLATE = 0x3856B74B, 0x5F0A43A7, 0xFFC5A970, 0x22ECBE63, 0x971A842E
ENTITY, MATERIAL, TEXTURE_SET, TEXTURE_MAP, MIP, MESH = (0x51DC6B80, 0x85C817C3, 0xD70E6670, 0xA2B7E917, 0x1D4B87A3,
                                                        0x415D9568)
# classes of nested objects
LOC_STRING, ITEM_BLOCK, ITEM_ICON, TAG = 0x81A7045D, 0x72873A8E, 0x38A922C1, 0xB332698E
BRANCH, TAG_LIST, TRAILER = 0x348B28D6, 0x11BD5345, 0xF5BD7B8A
REG_CATEGORY, REG_ENTRY = 0x0196529F, 0xA31AA51D
ENTITY_REF = 0xEC6AC357  # the class a selector child names for a mesh entity

BOOTSTRAP_ID, REGISTRY_ID = 0x800, 0xBD6
TEMPLATE_IDS = {"DataPC_extra_chr": [0x7A5370F8B7, 0x7A537127B6],  # STP_MF_Template, STP_F_Template
                "DataPC": [0xEAC340500B, 0xEAC3405050]}            # STP_M_NPC_Template, STP_F_NPC_Template
COSTUME_CATEGORY = 25
NEW_ID_BASE = 0x1F000000000  # above every shipped id (the highest is 0x1bd52dc0def); 4096 ids per added costume
ID_MIN = 1 << 35


def u32(b, o):
    return struct.unpack_from("<I", b, o)[0]


def u64(b, o):
    return struct.unpack_from("<Q", b, o)[0]


# --- the game's archives ------------------------------------------------------------------------------------------

class Game:
    """The game folder's archive stacks as the game ships them (base, _patch_01, _patch_02), newest copy first."""

    def __init__(self, folder: str):
        self.folder = folder
        self._stacks: dict[str, list[forge.Forge]] = {}
        self._files: dict[tuple[str, int], tuple[str, list[forge.Resource]]] = {}

    def stack(self, base: str) -> list[forge.Forge]:
        if base not in self._stacks:
            paths = [p for p in forge.patch_stack(os.path.join(self.folder, base + ".forge")) if os.path.exists(p)
                     and int((re.search(r"_patch_(\d+)\.forge$", p) or [0, 0])[1]) <= 2]
            if not paths:
                sys.exit(f"no {base}.forge in {self.folder}")
            self._stacks[base] = [forge.Forge(p) for p in reversed(paths)]
        return self._stacks[base]

    def entry(self, base: str, eid: int) -> tuple[forge.Forge, forge.Entry] | None:
        for fg in self.stack(base):
            if eid in fg.by_id:
                return fg, fg.by_id[eid]
        return None

    def datafile(self, base: str, eid: int) -> tuple[str, list[forge.Resource]]:
        """(archive name, resources) of the newest copy of an entry."""
        if (base, eid) not in self._files:
            found = self.entry(base, eid)
            if not found:
                sys.exit(f"{eid:#x}: in no archive of the {base} stack")
            fg, e = found
            self._files[base, eid] = (os.path.basename(fg.path), forge.parse_datafile(fg.read(e)).resources)
        return self._files[base, eid]

    def prefetch(self, base: str, eid: int) -> bytes | None:
        for fg in self.stack(base):
            if eid in fg.by_id:
                return forge.read_prefetch(fg).get(eid)
        return None

    def all_resource_ids(self) -> set[int]:
        """Every resource id of every archive (header blocks only; about 15 s)."""
        ids: set[int] = set()
        for p in sorted(glob.glob(os.path.join(self.folder, "*.forge"))):
            if int((re.search(r"_patch_(\d+)\.forge$", p) or [0, 0])[1]) > 2:
                continue
            fg = forge.Forge(p)
            for e in fg.entries:
                if e.id not in (forge.META_ID, forge.PREFETCH_ID):
                    ids.update(forge.header_ids(fg, e))
        return ids


# --- items ------------------------------------------------------------------------------------------------------

def parse_item(b: bytes) -> dict:
    """An item's fields with their offsets (raises on the ~5% of items whose layout differs, see ITEMS.md)."""
    d: dict = {}
    d["id"], cls = struct.unpack_from("<QI", b, 0)
    if cls != ITEM:
        raise ValueError(f"not an item ({cls:#x})")
    d["head"] = b[12:19]
    o = 19
    d["loc_obj"], c, d["loc"], n = struct.unpack_from("<QIII", b, o)
    if c != LOC_STRING:
        raise ValueError("no LocString at 0x13")
    d["loc_off"] = o + 12  # the name's string id (loc.py)
    o += 20
    d["debug_name"] = b[o:o + 2 * n].decode("utf-16-le") if n else ""
    o += 2 * n + 2 if n else 0
    n = u32(b, o + 1)
    d["unlock"] = [u64(b, o + 6 + 9 * k) for k in range(n)]  # unlock conditions (0x79bece6d resources)
    o += 5 + 9 * n
    d["block_obj"], c = struct.unpack_from("<QI", b, o)
    if c != ITEM_BLOCK:
        raise ValueError("no item block")
    o += 12
    d["block_off"] = o
    d["price_off"], d["default_off"], d["none_off"] = o + 0x3E, o + 0x4A, o + 0x4B
    o += 96
    d["icons"] = []
    for _ in range(5):
        oid, c, kind, z1, la, z2, lb = struct.unpack_from("<QIBBQBQ", b, o)
        if c != ITEM_ICON or z1 or z2:
            raise ValueError("unexpected item block layout")
        d["icons"].append((oid, kind, la, lb))
        o += 31
    d["flags3"] = b[o:o + 3]
    o += 3
    d["tags"], d["tag_offs"] = [], []
    for _ in range(7):
        n = u32(b, o)
        o += 4
        lst = []
        for _ in range(n):
            z, c, t = struct.unpack_from("<QII", b, o)
            if z or c != TAG:
                raise ValueError("bad tag record")
            lst.append(t)
            d["tag_offs"].append(o + 12)
            o += 16
        d["tags"].append(lst)
    n = u32(b, o)
    d["variants"] = [u64(b, o + 5 + 9 * k) for k in range(n)]
    o += 4 + 9 * n
    d["category"] = u32(b, o)
    d["tail_off"] = o + 4
    return d


TAG_LIST_NAMES = ("female", "male", "both", "list3", "list4", "list5", "list6")  # G: see ITEMS.md


def item_summary(b: bytes) -> str:
    try:
        d = parse_item(b)
    except (ValueError, struct.error) as ex:
        return f"(layout not decoded: {ex})"
    price = u32(b, d["price_off"])
    flags = (" default" if b[d["default_off"]] else "") + (" none-item" if b[d["none_off"]] else "")
    return f"category={d['category']:2} price={price:6} loc={d['loc']:#06x}{flags}"


# --- selectors --------------------------------------------------------------------------------------------------

def selector_branches(b: bytes, sid: int) -> tuple[int, int, list[dict]] | None:
    """(count offset, end offset, branches) of a selector. A branch: u64 id, class, f32 weight, u8 1, u64 owner,
    tag record (u64 0, class, u32 tag), tag list object (u64 id, class, u32 n, n tag records), children."""
    first = None
    for o in range(12, len(b) - 25):
        if u32(b, o + 8) == BRANCH and b[o + 16] == 1 and u64(b, o + 17) == sid:
            first = o
            break
    if first is None:
        return None
    o = first - 4
    n = u32(b, o)
    o += 4
    brs = []
    for _ in range(n):
        start = o
        bid, cls, w, one, owner, z, tc, tag = struct.unpack_from("<QIfBQQII", b, o)
        if cls != BRANCH or one != 1 or owner != sid or z or tc != TAG:
            raise ValueError(f"selector {sid:#x}: branch layout")
        o += 41
        lid, lc, m = struct.unpack_from("<QII", b, o)
        if lc != TAG_LIST:
            raise ValueError(f"selector {sid:#x}: tag list layout")
        o += 16
        tags = [u32(b, o + 16 * j + 12) for j in range(m)]
        o += 16 * m
        k = u32(b, o)
        o += 4
        children = []
        for _ in range(k):
            slot, ccls, z1, kind, z2 = struct.unpack_from("<IIHHI", b, o)
            if z1 or z2 or kind not in (0x12, 0x1C):
                raise ValueError(f"selector {sid:#x}: child kind {kind:#x}")
            o += 16 + (2 if kind == 0x1C else 1)
            children.append({"slot": slot, "class": ccls, "kind": kind, "target": u64(b, o), "target_off": o})
            o += 8
        brs.append({"id": bid, "weight": w, "tag": tag, "tag_off": start + 37, "list_id": lid, "tags": tags,
                    "children": children, "start": start, "end": o})
    return first - 4, o, brs


def link_lists_with(b: bytes, eid: int) -> list[tuple[int, list[int]]]:
    """(count offset, ids) of each link list (u32 n, n x (u8 0, u64 id)) in a payload that holds eid."""
    pat = b"\x00" + struct.pack("<Q", eid)
    out, seen = [], set()
    i = b.find(pat)
    while i >= 0:
        s = i
        while s - 9 >= 4 and b[s - 9] == 0 and ID_MIN <= u64(b, s - 8) < (1 << 42):
            s -= 9
        for start in range(s, i + 1, 9):
            n = u32(b, start - 4)
            if start <= i < start + 9 * n <= len(b) and all(b[start + 9 * k] == 0 for k in range(n)):
                if start - 4 not in seen:
                    seen.add(start - 4)
                    out.append((start - 4, [u64(b, start + 1 + 9 * k) for k in range(n)]))
                break
        i = b.find(pat, i + 1)
    return out


def closure_list(b: bytes) -> list[int]:
    """The list every selector ends with: the selectors below it (depth first, each once)."""
    for n in range(len(b) // 9 + 1):
        p = len(b) - 9 * n - 4
        if p < 0:
            break
        if u32(b, p) == n and all(b[p + 4 + 9 * k] == 0 for k in range(n)):
            return [u64(b, p + 5 + 9 * k) for k in range(n)]
    return []


def insert_links(b: bytes, count_off: int, index: int, ids: list[int]) -> bytes:
    n = u32(b, count_off)
    at = count_off + 4 + 9 * index
    return (b[:count_off] + struct.pack("<I", n + len(ids)) + b[count_off + 4:at]
            + b"".join(b"\x00" + struct.pack("<Q", i) for i in ids) + b[at:])


# --- cloning ------------------------------------------------------------------------------------------------------

class Ids:
    """New ids for one added costume: a block of 4096 above every shipped id, chosen by the costume's name."""

    def __init__(self, name: str, taken: set[int]):
        self.next = NEW_ID_BASE + (zlib.crc32(name.encode()) & 0xFFFFF) * 0x1000
        self.end = self.next + 0x1000
        self.taken = taken
        self.map: dict[int, int] = {}

    def new(self, old: int | None = None) -> int:
        if old is not None and old in self.map:
            return self.map[old]
        while self.next in self.taken:
            self.next += 1
        if self.next >= self.end:
            sys.exit("more than 4096 new ids for one costume")
        v = self.next
        self.next += 1
        self.taken.add(v)
        if old is not None:
            self.map[old] = v
        return v


# Classes of the nested objects that carry ids of their own, in the payload types add-costume clones (item, selector,
# entity, material). Counted over all items and selectors and 400 character entries: every u64 that is no resource
# but sits in the id range and is followed by one of these is an object id. Without the list, bool arrays such as
# 01 01 01 00 01 01 00 00 in a material look like ids too.
NESTED_CLASSES = {LOC_STRING, ITEM_BLOCK, ITEM_ICON, 0x7E42F87F,                       # item
                  BRANCH, TAG_LIST, TRAILER, 0x36839608, 0xBC5B87AF, 0xD9606976, 0x2D7B69F6, 0x731CE65C,
                  0xA9CC15E8,                                                           # selector
                  0xA360319A,                                                           # entity (its LODs)
                  0x7D08460D}                                                           # material


def nested_ids(b: bytes, resource_ids: set[int]) -> list[int]:
    """Ids of a payload's nested objects: u64 ids that are no resource anywhere, followed by a nested object's class
    hash. The payload's own id (offset 0) is left to the caller."""
    out = []
    for o in range(8, len(b) - 11):
        v = u64(b, o)
        if (ID_MIN <= v < (1 << 42) and v not in resource_ids and u32(b, o + 8) in NESTED_CLASSES
                and v not in out):
            out.append(v)
    return out


def remap(b: bytes, idmap: dict[int, int], window: int = 0x10000) -> bytes:
    """Every id of idmap replaced (8-byte values). In a big payload (mesh, texture data) only its first and last
    `window` bytes, where its ids are (a mesh's own id at 0 and its materials near the end; a texture's links)."""
    if len(b) > 4 * window:
        return remap(b[:window], idmap) + b[window:-window] + remap(b[-window:], idmap)
    for s, d in idmap.items():
        b = b.replace(struct.pack("<Q", s), struct.pack("<Q", d))
    return b


def remap_prefetch(blob: bytes | None, idmap: dict[int, int]) -> bytes | None:
    """A prefetch blob (u16, u16 k, k x (u64 id, 3 bytes)) with its dependency ids remapped."""
    if not blob or len(blob) < 4:
        return blob
    out = bytearray(blob)
    for k in range(struct.unpack_from("<H", blob, 2)[0]):
        o = 4 + 11 * k
        struct.pack_into("<Q", out, o, idmap.get(u64(blob, o), u64(blob, o)))
    return bytes(out)


def source_override(dirs: list[str], eid: int) -> list[forge.Resource] | None:
    """An entry from a --source-dir (e.g. another mod's generated folders) instead of the game's copy."""
    for d in dirs:
        for p in glob.glob(os.path.join(d, f"{eid:016x}_*")):
            if os.path.isdir(p):
                return forge.load_unpacked(p)[1]
            if p.endswith(".datafile"):
                with open(p, "rb") as f:
                    return forge.parse_datafile(f.read()).resources
    return None


# --- commands -----------------------------------------------------------------------------------------------------

def load_bootstrap(game: Game) -> tuple[str, list[forge.Resource]]:
    return game.datafile("DataPC", BOOTSTRAP_ID)


def by_name(res: list[forge.Resource], spec: str) -> forge.Resource:
    if re.fullmatch(r"0x[0-9a-fA-F]+", spec):
        hit = [r for r in res if r.id == int(spec, 16)]
    else:
        hit = [r for r in res if r.name == spec and r.type == ITEM] or [r for r in res if r.name == spec]
    if not hit:
        sys.exit(f"{spec}: no such resource")
    return hit[0]


def cmd_list(a):
    _, res = load_bootstrap(Game(a.game))
    for r in res:
        if r.type != ITEM or (a.grep and a.grep.lower() not in r.name.lower()):
            continue
        s = item_summary(r.payload)
        if a.category is not None and f"category={a.category:2} " not in s:
            continue
        print(f"{r.id:#014x}  {r.name:50} {s}")


def costume_chain(game: Game, tags: set[int]) -> list[str]:
    """Template branches keyed by one of these tags, with what they select."""
    _, tres = game.datafile("DataPC_extra_chr", TEMPLATE_IDS["DataPC_extra_chr"][0])
    tby = {r.id: r for r in tres}
    out = []
    for r in tres:
        if r.type != SELECTOR:
            continue
        try:
            got = selector_branches(r.payload, r.id)
        except ValueError:
            continue
        for br in got[2] if got else []:
            if br["tag"] in tags and br["tag"]:  # tag 0: a selector's default branch
                kids = [(tby[c["target"]].name if c["target"] in tby else f"{c['target']:#x} (entry)")
                        for c in br["children"]]
                out.append(f"tag {br['tag']:#010x}: {r.name} -> {', '.join(kids)}")
    return out


def cmd_show(a):
    game = Game(a.game)
    arch, res = load_bootstrap(game)
    byid = {r.id: r for r in res}
    r = by_name(res, a.item)
    b = r.payload
    print(f"{r.name}  id={r.id:#x}  type={r.type:#010x}  {len(b)} bytes  (Game Bootstrap Settings, {arch})")
    try:
        d = parse_item(b)
    except (ValueError, struct.error) as ex:
        print(f"  layout not decoded ({ex})")
        d = None
    if d:
        print(f"  {item_summary(b)}  debug name={d['debug_name']!r}  head={d['head'].hex()} flags3={d['flags3'].hex()}")
        for u in d["unlock"]:
            print(f"  unlock condition: {u:#x} {byid[u].name if u in byid else '?'}")
        for oid, kind, la, lb in d["icons"]:
            if la or lb:
                print(f"  icon object {oid:#x} kind {kind}: {la:#x} {lb:#x}")
        for name, lst in zip(TAG_LIST_NAMES, d["tags"]):
            if lst:
                print(f"  tags[{name}]: {' '.join(f'{t:#010x}' for t in lst)}")
        for v in d["variants"]:
            print(f"  variant: {v:#x} {byid[v].name if v in byid else '?'}")
        tail = b[d["tail_off"]:]
        for o in range(len(tail) - 7):
            v = u64(tail, o)
            if v in byid and v != r.id:
                print(f"  tail +{o:#x}: {v:#x} {byid[v].name}")
        for line in costume_chain(game, {t for lst in d["tags"] for t in lst}):
            print(f"  template: {line}")
    pat = struct.pack("<Q", r.id)
    for x in res:
        if x is not r and pat in x.payload:
            print(f"  referenced by {x.name} ({x.type:#010x})")


def cmd_costumes(a):
    game = Game(a.game)
    _, res = load_bootstrap(game)
    _, tres = game.datafile("DataPC_extra_chr", TEMPLATE_IDS["DataPC_extra_chr"][0])
    tby = {r.id: r for r in tres}
    items_by_tag: dict[int, list[str]] = {}
    for r in res:
        if r.type == ITEM:
            try:
                for lst in parse_item(r.payload)["tags"]:
                    for t in lst:
                        items_by_tag.setdefault(t, []).append(r.name)
            except (ValueError, struct.error):
                pass
    for name in ("STP_M_Clothing", "STP_F_Clothing"):
        sel = next(r for r in tres if r.name == name)
        print(name)
        for br in selector_branches(sel.payload, sel.id)[2]:
            kids = []
            for c in br["children"]:
                t = tby.get(c["target"])
                if t and t.type == SELECTOR:
                    ent = [f"{x['target']:#x}" for b2 in (selector_branches(t.payload, t.id) or (0, 0, []))[2]
                           for x in b2["children"] if x["class"] == ENTITY_REF]
                    kids.append(t.name + (f" (entity {', '.join(sorted(set(ent)))})" if ent else ""))
            items = sorted(set(items_by_tag.get(br["tag"], []))) if br["tag"] else ["(the default branch)"]
            print(f"  tag {br['tag']:#010x} -> {', '.join(kids)}; items: {', '.join(items) or '-'}")


@dataclass
class CostumeSpec:
    """One costume to add: a clone of `like` (an item) or of the item drawing `costume` (a costume selector)."""
    name: str
    like: str | None = None
    costume: str | None = None
    label: str | None = None
    source_dirs: list[str] = field(default_factory=list)  # entries to clone from instead of the game's (a mod's model)
    keep_price: bool = False
    title: str | None = None


@dataclass
class CostumePlan:
    """What adding one costume changes, worked out before anything is written (so several share one write)."""
    spec: CostumeSpec
    src: forge.Resource                    # the source item
    item: forge.Resource                   # the new item
    model_tag: int
    new_tag: int
    costume_sel: dict                      # gender -> the source's costume selector
    clothing: dict                         # gender -> the STP_<g>_Clothing selector (shipped copy)
    tex_of: dict                           # gender -> texture selector ids of that costume
    clone_map: dict[int, int]
    new_sels: dict[int, forge.Resource]    # source selector id -> its clone
    branch_ids: dict
    plan: dict                             # entry id -> (source, resources) of the entries cloned
    entities: dict
    key: str
    newkey: str
    title_id: int | None
    report: dict


def plan_costume(game: Game, ctx: dict, spec: CostumeSpec) -> CostumePlan:
    """The changes for one costume; `ctx` holds what every costume of a run shares and reserves (ids, tags,
    string ids, the shipped bootstrap and template)."""
    name = spec.name
    if not re.fullmatch(r"[A-Za-z0-9_]{4,100}", name):
        sys.exit(f"{name}: a name is 4-100 letters, digits, underscores")
    label = spec.label or re.sub(r"^COSTUMES?_(MF?_|F_)?", "", name).title().replace("_", "")[:24]
    resource_ids, bres, tres, tby = ctx["resource_ids"], ctx["bres"], ctx["tres"], ctx["tby"]
    ids = Ids(name, ctx["taken"])
    report: dict = {"name": name, "like": spec.like}
    like = spec.like
    if not like:
        like = item_for_costume(game, bres, spec.costume)
        print(f"  {spec.costume}: cloning {like}")
        report["like"] = like
    src = by_name(bres, like)
    if any(r.name == name for r in bres):
        sys.exit(f"{name}: an item of that name exists")
    d = parse_item(src.payload)
    if d["category"] != COSTUME_CATEGORY:  # e.g. JACKET_MF_C_Penguin_228, the part of a costume bundle that draws it
        print(f"  note: {like} is no costume item (category {d['category']}); the new item gets its category")
    item_tags = {t for lst in d["tags"] for t in lst}
    clothing = ctx["clothing"]
    model_tag, costume_sel = None, {}
    for g, sel in clothing.items():
        for br in selector_branches(sel.payload, sel.id)[2]:
            if br["tag"] in item_tags and br["tag"]:
                kid = next((c["target"] for c in br["children"] if c["class"] == SELECTOR), None)
                if kid:
                    model_tag, costume_sel[g] = br["tag"], tby[kid]
    if not model_tag or set(costume_sel) != {"M", "F"}:
        sys.exit(f"{like}: none of its tags selects a costume in both STP_M_Clothing and STP_F_Clothing")
    new_tag = zlib.crc32(f"POWDER:{name}".encode())
    registry = by_name(bres, f"{REGISTRY_ID:#x}").payload
    while new_tag in ctx["taken_tags"] or struct.pack("<I", new_tag) in registry:
        new_tag = (new_tag + 1) & 0xFFFFFFFF
    ctx["taken_tags"].add(new_tag)
    report["model_tag"], report["new_tag"] = f"{model_tag:#010x}", f"{new_tag:#010x}"

    item_id = ids.new(src.id)
    p = bytearray(src.payload)
    for o in d["tag_offs"]:
        if u32(p, o) == model_tag:
            struct.pack_into("<I", p, o, new_tag)
    if not spec.keep_price:
        struct.pack_into("<I", p, d["price_off"], 0)
        p[d["default_off"]] = 1  # G: set on the stock riders' starter gear (see ITEMS.md)
    title_id = None
    if spec.title:  # a name of its own: a new string id, its text added to every UI language package (loc.py)
        title_id = loc.new_string_id(name, ctx["loc_taken"])
        ctx["loc_taken"].add(title_id)
        struct.pack_into("<I", p, d["loc_off"], title_id)
        report.update(title=spec.title, loc=f"{title_id:#x}", loc_was=f"{d['loc']:#x}")
    item_map = {src.id: item_id}
    for v in nested_ids(bytes(p), resource_ids):
        item_map[v] = ids.new(v)
    new_item = forge.Resource(item_id, ITEM, name, remap(bytes(p), item_map), src.flag, src.pre, src.extra)
    parse_item(new_item.payload)
    report["item"] = f"{item_id:#x}"
    report["gear_list"] = next((r.name for r in bres if r.type == GEAR_LIST and struct.pack("<Q", src.id) in r.payload),
                               None)
    if not report["gear_list"]:
        sys.exit(f"{like}: in no GEAR_* list")

    # the template side: new costume selectors (M, F) and texture selector, a new branch per Clothing selector
    sel_map: dict[int, int] = {}
    tex_sel, tex_of = {}, {"M": [], "F": []}  # costume 016 has one texture selector for both, 010 one per gender
    for g, s in costume_sel.items():
        sel_map[s.id] = ids.new(s.id)
        for br in selector_branches(s.payload, s.id)[2]:
            for c in br["children"]:
                t = tby.get(c["target"])
                if t is not None and t.type == SELECTOR and t.name.endswith("_Textures"):
                    tex_sel[t.id] = t
                    tex_of[g].append(t.id)
    for t in tex_sel.values():
        sel_map[t.id] = ids.new(t.id)
    key = re.sub(r"^STP_[MF]_", "", costume_sel["M"].name)  # e.g. Costume_016
    newkey = f"Costume_{label}"

    # entities and their own materials/textures: everything named after the costume gets a new id
    entities = {}
    for g, s in costume_sel.items():
        for br in selector_branches(s.payload, s.id)[2]:
            for c in br["children"]:
                if c["class"] == ENTITY_REF:
                    entities[c["target"]] = g
    materials = set()
    for t in tex_sel.values():
        for br in selector_branches(t.payload, t.id)[2]:
            if br["tag"] in item_tags:
                materials.update(c["target"] for c in br["children"] if c["class"] == MATERIAL)
    own = re.compile(rf"_{re.escape(key)}(_|$)")
    plan: dict[int, tuple[str, list[forge.Resource]]] = {}  # entry id -> (source, resources)
    todo = list(entities) + sorted(materials)
    while todo:
        eid = todo.pop()
        if eid in plan:
            continue
        found = game.entry("DataPC_extra_chr", eid)
        if not found:
            continue
        fg, e = found
        if eid not in entities and not own.search(e.name):
            continue  # shared (e.g. STP_MF_Glass_ALL): referenced as it is
        over = source_override(spec.source_dirs, eid)
        res = over if over is not None else game.datafile("DataPC_extra_chr", eid)[1]
        plan[eid] = ("source-dir" if over is not None else os.path.basename(fg.path), res)
        for r in res:  # entries it refers to: a material's textures, a texture's Mips (links near the start)
            head = r.payload if r.type in (MATERIAL, TEXTURE_SET, ENTITY) else r.payload[:0x100]
            if r.type not in (MATERIAL, TEXTURE_SET, ENTITY, TEXTURE_MAP):
                continue
            for o in range(len(head) - 7):
                v = u64(head, o)
                if v != eid and v in resource_ids and v not in plan:
                    f2 = game.entry("DataPC_extra_chr", v)
                    if f2 and own.search(f2[1].name):
                        todo.append(v)
    if spec.source_dirs and not any(how == "source-dir" for how, _ in plan.values()):
        sys.exit(f"{name}: its source dirs hold none of the entries {costume_sel['M'].name} is made of")
    clone_map: dict[int, int] = dict(sel_map)
    for eid, (_, res) in plan.items():
        for r in res:
            if r.id == eid or own.search(r.name):
                ids.new(r.id)
    clone_map.update(ids.map)
    for eid, (_, res) in plan.items():
        for r in res:
            if r.id in clone_map and r.type in (ENTITY, MATERIAL, TEXTURE_SET):
                for v in nested_ids(r.payload, resource_ids):
                    clone_map[v] = ids.new(v)
    for s in list(costume_sel.values()) + list(tex_sel.values()):
        for v in nested_ids(s.payload, resource_ids):
            clone_map[v] = ids.new(v)
    clone_map.update(ids.map)

    # the texture selector's other branches (other colourways) keep pointing at the source's materials
    new_sels = {s.id: forge.Resource(clone_map[s.id], SELECTOR, s.name.replace(key, newkey),
                                     remap(s.payload, clone_map), s.flag, s.pre, s.extra)
                for s in list(costume_sel.values()) + list(tex_sel.values())}
    report["selectors"] = {v.name: f"{v.id:#x}" for v in new_sels.values()}
    branch_ids = {g: (ids.new(), ids.new()) for g in "MF"}  # the same in every template copy
    report["entities"] = {f"{clone_map[e]:#x}": plan[e][1][0].name.replace(key, newkey) for e in entities}
    report["cloned_entries"] = len(plan)
    report["source_dirs"] = [os.path.abspath(x) for x in spec.source_dirs]
    report["ids"] = {f"{s:#x}": f"{d:#x}" for s, d in sorted(clone_map.items())}
    return CostumePlan(spec, src, new_item, model_tag, new_tag, costume_sel, clothing, tex_of, clone_map, new_sels,
                       branch_ids, plan, entities, key, newkey, title_id, report)


def patch_template(res: list[forge.Resource], c: CostumePlan) -> list[forge.Resource]:
    """A character template datafile with one costume's branch, selectors and list entries added."""
    out = []
    for r in res:
        pl = r.payload
        for g in "MF":  # lists may name selectors this datafile lacks (STP_MF_Parts in the M NPC template)
            s, cl = c.costume_sel[g], c.clothing[g]
            if r.id == cl.id:
                pl = add_branch(pl, cl.id, c.model_tag, c.new_tag, s.id, c.clone_map[s.id], c.branch_ids[g])
            add = [c.clone_map[s.id]] + [c.clone_map[t] for t in dict.fromkeys(c.tex_of[g])]  # depth first
            span = set(closure_list(cl.payload)) | {cl.id}
            for count_off, lst in link_lists_with(pl, s.id):
                if r.id == s.id:
                    continue
                at = max(k for k, v in enumerate(lst) if v in span) + 1
                pl = insert_links(pl, count_off, at, [v for v in add if v not in lst])
        out.append(forge.Resource(r.id, r.type, r.name, pl, r.flag, r.pre, r.extra) if pl is not r.payload else r)
        for old, new in c.new_sels.items():
            if r.id == old:
                out.append(new)
    return out


def add_costumes(game: Game, specs: list[CostumeSpec], out: str, level: int = 9) -> list[CostumePlan]:
    """Add every costume of `specs` in one go: the shared resources (Game Bootstrap Settings, the language
    packages, the four character templates) are patched once with all of them, each costume's own entries are
    cloned under its own ids. Writes `<out>/DataPC/`, `<out>/DataPC_extra_chr/` and a `<name>.json` per costume."""
    names = [sp.name for sp in specs]
    if len(set(names)) != len(names):
        sys.exit(f"the same item name twice: {sorted({n for n in names if names.count(n) > 1})}")
    print("reading the archives' resource ids (about 15 s)")
    resource_ids = game.all_resource_ids()
    _, bres = load_bootstrap(game)
    _, tres = game.datafile("DataPC_extra_chr", TEMPLATE_IDS["DataPC_extra_chr"][0])
    ctx = {"resource_ids": resource_ids, "taken": set(resource_ids), "bres": bres, "tres": tres,
           "tby": {r.id: r for r in tres},
           "clothing": {g: next(r for r in tres if r.name == f"STP_{g}_Clothing") for g in "MF"},
           "taken_tags": {t for r in bres if r.type == ITEM for t in _item_tags(r.payload)},
           "loc_taken": loc.all_ids(game.folder) if any(sp.title for sp in specs) else set()}
    plans = [plan_costume(game, ctx, sp) for sp in specs]

    # 1. Game Bootstrap Settings: the new items (each after its source), the gear lists, the tag registry
    new_bres = []
    for r in bres:
        pl = r.payload
        for c in plans:
            if r.type == GEAR_LIST and r.name == c.report["gear_list"]:
                pl = append_gear(pl, c.item.id)
            elif r.id == REGISTRY_ID:
                pl = register_tag(pl, c.model_tag, c.new_tag)
        new_bres.append(r if pl is r.payload else forge.Resource(r.id, r.type, r.name, pl, r.flag, r.pre, r.extra))
        new_bres += [c.item for c in plans if c.src.id == r.id]
    out_dc, out_chr = os.path.join(out, "DataPC"), os.path.join(out, "DataPC_extra_chr")
    os.makedirs(out_dc, exist_ok=True)
    os.makedirs(out_chr, exist_ok=True)
    written = [write_datafile(out_dc, BOOTSTRAP_ID, "Game Bootstrap Settings", new_bres, level)]

    # 2. the names: every title in one rewrite of each language package
    titles = {c.title_id: c.spec.title for c in plans if c.title_id is not None}
    if titles:
        pk = loc.set_strings(game.folder, titles, out_dc)
        written += pk
        for c in plans:
            if c.title_id is not None:
                c.report["loc_packages"] = len(pk)

    # 3. the character templates, each patched with every costume
    for base, tids in TEMPLATE_IDS.items():
        for tid in tids:
            _, res = game.datafile(base, tid)
            mine = [c for c in plans if any(r.id in (c.costume_sel["M"].id, c.costume_sel["F"].id) for r in res)]
            if not mine:
                continue
            for c in mine:
                res = patch_template(res, c)
            written.append(write_datafile(out_dc if base == "DataPC" else out_chr, tid, res[0].name, res, level))

    # 4. each costume's cloned entries (entities, materials, textures) under its new ids, with their prefetch blobs
    for c in plans:
        for eid, (srcname, res) in sorted(c.plan.items()):
            new_res = []
            for r in res:  # shared resources carried along (e.g. STP_MF_Glass_ALL) stay as they are
                nid = c.clone_map.get(r.id, r.id)
                if nid == r.id:
                    new_res.append(r)
                    continue
                new_res.append(forge.Resource(nid, r.type, r.name.replace(c.key, c.newkey),
                                              remap(r.payload, c.clone_map), r.flag, r.pre, r.extra))
            blob = remap_prefetch(game.prefetch("DataPC_extra_chr", eid), c.clone_map)
            nid = c.clone_map[eid]
            dest = os.path.join(out_chr, f"{nid:016x}_{forge.safe_name(new_res[0].name)}")
            more = {"clone": {"from": f"{eid:#x}", "from_name": res[0].name, "source": srcname, "item": c.spec.name}}
            if blob is not None:
                more["prefetch"] = blob.hex()
            forge.write_unpacked(dest, nid, new_res[0].name, new_res[0].type, new_res, **more)
            written.append(dest)
        c.report["batch"] = names
        with open(os.path.join(out, f"{c.spec.name}.json"), "w") as f:
            json.dump(c.report, f, indent=1)
    for w in written:
        print(f"  {w}")
    for c in plans:
        print(f"{c.spec.name}: item {c.item.id:#x} in {c.report['gear_list']}, tag {c.new_tag:#010x} "
              f"(was {c.model_tag:#010x}), {len(c.new_sels)} selectors, {len(c.plan)} cloned entries")
        if c.title_id is not None:
            print(f"  name {c.spec.title!r}: string id {c.title_id:#x} in {c.report['loc_packages']} language packages")
    print(f"{len(plans)} costume(s) -> {out}")
    return plans


def read_specs(path: str) -> list[CostumeSpec]:
    """`[[costume]]` tables of a TOML file: name, like or costume, label, source (a folder or a list, relative to the
    file), title, keep_price."""
    import tomllib
    with open(path, "rb") as f:
        data = tomllib.load(f)
    here = os.path.dirname(os.path.abspath(path))
    specs = []
    for t in data.get("costume", []):
        srcs = t.get("source", [])
        srcs = [srcs] if isinstance(srcs, str) else list(srcs)
        if bool(t.get("like")) == bool(t.get("costume")):
            sys.exit(f"{path}: {t.get('name')}: give exactly one of like / costume")
        specs.append(CostumeSpec(name=t["name"], like=t.get("like"), costume=t.get("costume"), label=t.get("label"),
                                 source_dirs=[os.path.join(here, x) for x in srcs],
                                 keep_price=bool(t.get("keep_price", False)), title=t.get("title")))
    if not specs:
        sys.exit(f"{path}: no [[costume]] tables")
    return specs


def cmd_add_costume(a):
    add_costumes(Game(a.game), [CostumeSpec(name=a.name, like=a.like, costume=a.costume, label=a.label,
                                            source_dirs=a.source_dir, keep_price=a.keep_price, title=a.title)],
                 a.out, a.level)


def cmd_add_costumes(a):
    add_costumes(Game(a.game), read_specs(a.spec), a.out, a.level)


def cmd_check(a):
    """Re-reads what add-costume wrote and checks it against the game's own copies (exit 1 on a problem)."""
    game = Game(a.game)
    with open(a.report) as f:
        rep = json.load(f)
    out = os.path.dirname(os.path.abspath(a.report))
    others = []  # the other costumes written in the same run (add-costumes): their additions are expected too
    for path in glob.glob(os.path.join(out, "*.json")):
        if os.path.abspath(path) != os.path.abspath(a.report):
            with open(path) as f:
                o = json.load(f)
            if isinstance(o, dict) and "item" in o and "ids" in o:
                others.append(o)
    every = [rep] + others
    source_dirs = rep.get("source_dirs") or a.source_dir
    idmap = {int(s, 16): int(d, 16) for s, d in rep["ids"].items()}
    inv = {d: s for s, d in idmap.items()}
    new_ids = set(idmap.values())
    item_id, new_tag, old_tag = int(rep["item"], 16), int(rep["new_tag"], 16), int(rep["model_tag"], 16)
    problems = []

    def need(ok, msg):
        if not ok:
            problems.append(msg)
        return ok

    def load(path):
        if os.path.isdir(path):
            return forge.load_unpacked(path)[1]
        with open(path, "rb") as f:
            return forge.parse_datafile(f.read()).resources

    # the bootstrap: item, list, registry; everything else unchanged
    _, orig = load_bootstrap(game)
    new = load(glob.glob(os.path.join(out, "DataPC", f"{BOOTSTRAP_ID:016x}_*"))[0])
    need(len(new) == len(orig) + len(every), f"bootstrap: not exactly {len(every)} resource(s) more")
    nb = {r.id: r for r in new}
    item = nb.get(item_id)
    if need(item is not None, "bootstrap: no new item"):
        d = parse_item(item.payload)
        tags = {t for lst in d["tags"] for t in lst}
        need(new_tag in tags and old_tag not in tags, "item: model tag not replaced")
        need(not any(i in item.payload for i in (struct.pack("<Q", s) for s in idmap if s != item_id)),
             "item: still holds an id of the source's own objects")
    changed = [r.id for r in orig if nb[r.id].payload != r.payload]
    gear_ids = {next(r.id for r in orig if r.name == o["gear_list"]) for o in every}
    need(set(changed) == gear_ids | {REGISTRY_ID}, f"bootstrap: changed {[hex(c) for c in changed]}")
    gear = next(r for r in new if r.name == rep["gear_list"])
    append_gear(gear.payload, 0)  # still parses
    need(gear.payload.count(struct.pack("<Q", item_id)) == 1, "gear list: item not listed once")
    reg = nb[REGISTRY_ID].payload
    need(reg.count(struct.pack("<II", REG_ENTRY, new_tag)) == 1, "registry: new tag not registered once")

    # the name: the source's string id, or (--title) a new one whose text every UI language package gained
    src_item = next((r for r in orig if r.name == rep["like"]), None)
    if item is not None and src_item is not None:
        loc_id, src_loc = parse_item(item.payload)["loc"], parse_item(src_item.payload)["loc"]
        if "loc" not in rep:
            need(loc_id == src_loc, "item: string id differs from its source's")
        elif need(loc_id == int(rep["loc"], 16) and loc_id != src_loc, "item: LocString does not name the new id"):
            shipped = loc.packages(game.folder, subtitles=None)
            need(all(loc_id not in x.package.strings for x in shipped), f"string id {loc_id:#x} is a shipped one")
            for x in shipped:
                if x.name.endswith("_Subtitles"):
                    continue
                hit = [h for h in glob.glob(os.path.join(out, "DataPC", f"{x.entry.id:016x}_*"))
                       if h.endswith(".datafile") or os.path.isdir(h)]
                if not need(len(hit) == 1, f"{x.name}: not written once"):
                    continue
                q = loc.Package.parse(load(hit[0])[0].payload)
                e0 = x.package.entries
                need(q.head == x.package.head and q.b1 == x.package.b1, f"{x.name}: header changed")
                need(q.entries[:len(e0)] == e0 and all(y == 0 for _, y in q.entries[len(e0):]),
                     f"{x.name}: dictionary entries changed (only new leaves may be appended)")
                need(loc_id in q.strings and q.text(loc_id) == rep["title"], f"{x.name}: the name does not read back")
                titled = {int(o["loc"], 16) for o in every if "loc" in o}
                need({k: v for k, v in q.strings.items() if k not in titled} == x.package.strings,
                     f"{x.name}: other strings changed")

    # the templates: the new branch, the new selectors, the lists; shared resources identical in every copy
    sel_new = {int(v, 16) for v in rep["selectors"].values()}
    sel_all = {int(v, 16) for o in every for v in o["selectors"].values()}
    copies: dict[int, bytes] = {}
    for base, tids in TEMPLATE_IDS.items():
        for tid in tids:
            hit = glob.glob(os.path.join(out, base, f"{tid:016x}_*"))
            if not hit:
                continue
            res = load(hit[0])
            _, o_res = game.datafile(base, tid)
            have = {r.id for r in res}
            o_have = {r.id for r in o_res}
            need(o_have <= have and (have - o_have) <= sel_all, f"{tid:#x}: resources other than the new selectors")
            o_pl = {o.id: o.payload for o in o_res}
            for r in res:
                if r.id in copies and copies[r.id][0] == o_pl.get(r.id):  # copies that were the same still are
                    need(copies[r.id][1] == r.payload, f"{tid:#x}: {r.name} differs from its copy in another template")
                copies.setdefault(r.id, (o_pl.get(r.id), r.payload))
                if r.name in ("STP_M_Clothing", "STP_F_Clothing"):
                    brs = selector_branches(r.payload, r.id)[2]
                    nbr = [b for b in brs if b["tag"] == new_tag]
                    need(len(nbr) == 1 and any(c["target"] in sel_new for c in nbr[0]["children"]),
                         f"{r.name}: no branch for the new tag")
                for s, dd in idmap.items():
                    if r.id not in idmap and s in o_have and dd in sel_new:
                        for _, lst in link_lists_with(r.payload, s):
                            need(dd in lst, f"{r.name}: a list with {s:#x} lacks {dd:#x}")
            for r in res:
                if r.id in sel_new:
                    need(selector_branches(r.payload, r.id) is not None, f"{r.name}: branches do not parse")
                    for s in idmap:
                        if s in o_have and struct.pack("<Q", s) in r.payload:
                            need(False, f"{r.name}: still refers to the source's selector {s:#x}")

    # the cloned entries: new ids only for their own resources, no old own id left, references resolve
    known = {int(d, 16) for o in every for d in o["ids"].values()} | set(game.all_resource_ids())
    n = 0
    for p in sorted(glob.glob(os.path.join(out, "DataPC_extra_chr", "*"))):
        if not os.path.isdir(p):
            continue
        m, res = forge.load_unpacked(p)
        if m.get("clone", {}).get("item", rep["name"]) != rep["name"]:
            continue  # another costume's entry
        n += 1
        need(res[0].id in new_ids, f"{p}: entry id is not new")
        src_id = int(m["clone"]["from"], 16)
        over = source_override(source_dirs, src_id)
        src = {r.id: r for r in (over if over is not None else game.datafile("DataPC_extra_chr", src_id)[1])}
        for r in res:
            s = src.get(inv.get(r.id, r.id))
            if not need(s is not None and len(s.payload) == len(r.payload), f"{r.name}: no source of its size"):
                continue
            if r.id not in new_ids:
                need(s.payload == r.payload, f"{os.path.basename(p)}: shared {r.name} changed")
            elif len(r.payload) <= 0x40000:  # every changed byte lies in an 8-byte id the map replaced
                o = 0
                while o < len(r.payload):
                    if r.payload[o] != s.payload[o]:
                        k = next((k for k in range(max(0, o - 7), o + 1) if idmap.get(u64(s.payload, k)) ==
                                  u64(r.payload, k)), None)
                        if not need(k is not None, f"{r.name}: byte {o:#x} changed but is no remapped id"):
                            break
                        o = k + 8
                    else:
                        o += 1
            head = r.payload if len(r.payload) <= 0x40000 else r.payload[:0x10000] + r.payload[-0x10000:]
            for s in idmap:
                if s not in sel_new and struct.pack("<Q", s) in head and idmap[s] != s:
                    need(False, f"{os.path.basename(p)}: {r.name} still holds {s:#x}")
            if r.type in (ENTITY, MATERIAL, TEXTURE_SET):
                for o in range(0, len(r.payload) - 7):
                    v = u64(r.payload, o)
                    if NEW_ID_BASE <= v < NEW_ID_BASE + (1 << 32) and v not in known:
                        need(False, f"{r.name}: refers to {v:#x}, which is nowhere")
        if "prefetch" in m:
            bl = bytes.fromhex(m["prefetch"])
            for k in range(struct.unpack_from("<H", bl, 2)[0] if len(bl) >= 4 else 0):
                v = u64(bl, 4 + 11 * k)
                need(v in known, f"{os.path.basename(p)}: prefetch names {v:#x}, which is nowhere")
    for msg in problems:
        print(f"  problem: {msg}")
    print(f"{rep['name']}: {n} cloned entries, {len(copies)} template resources checked, "
          f"{len(problems)} problem(s)")
    return 1 if problems else 0


def item_for_costume(game: Game, bres: list[forge.Resource], costume: str) -> str:
    """The item that draws a costume selector (e.g. STP_M_Costume_005): one carrying the tag of its Clothing
    branch, a costume item (category 25) if there is one."""
    _, tres = game.datafile("DataPC_extra_chr", TEMPLATE_IDS["DataPC_extra_chr"][0])
    tby = {r.id: r for r in tres}
    tag = None
    for name in ("STP_M_Clothing", "STP_F_Clothing"):
        sel = next(r for r in tres if r.name == name)
        for br in selector_branches(sel.payload, sel.id)[2]:
            if br["tag"] and any(tby.get(c["target"]) is not None and tby[c["target"]].name == costume
                                 for c in br["children"]):
                tag = br["tag"]
    if tag is None:
        sys.exit(f"{costume}: no branch of STP_M_Clothing / STP_F_Clothing selects it")
    hits = []
    for r in bres:
        if r.type == ITEM and tag in _item_tags(r.payload):
            hits.append((parse_item(r.payload)["category"] != COSTUME_CATEGORY, r.name))
    if not hits:
        sys.exit(f"{costume}: no item carries its tag {tag:#010x}")
    return min(hits)[1]


def _item_tags(b: bytes) -> set[int]:
    try:
        return {t for lst in parse_item(b)["tags"] for t in lst}
    except (ValueError, struct.error):
        return set()


def append_gear(b: bytes, item_id: int) -> bytes:
    """A GEAR_* list with one more item: u64 id, class, u8 1, tag record, u32 category, then two lists of
    (u8 link kind, u8 0, u64 id): the slot's "none" item and all items. The second list ends the payload."""
    o = 33
    n0 = u32(b, o)
    o += 4 + 10 * n0
    n1 = u32(b, o)
    if o + 4 + 10 * n1 != len(b):
        raise ValueError("GEAR list layout")
    return b[:o] + struct.pack("<I", n1 + 1) + b[o + 4:] + b"\x01\x00" + struct.pack("<Q", item_id)


def register_tag(b: bytes, like: int, new: int) -> bytes:
    """The tag registry with `new` added to the category that holds `like` (entries: u64 0, class, u32 tag,
    u32 group, u8 0; the category: u64 id, class, u8 1, u32 n)."""
    ent = next(o - 8 for o in range(len(b) - 8) if u32(b, o) == REG_ENTRY and u32(b, o + 4) == like)
    cat = max(o - 8 for o in range(ent) if u32(b, o) == REG_CATEGORY)
    n = u32(b, cat + 13)
    first = cat + 17
    for k in range(n):
        o = first + 21 * k
        if u32(b, o + 8) != REG_ENTRY or b[o + 20] != 0:
            raise ValueError("tag registry: category with nested entries")
    end = first + 21 * n
    group = u32(b, ent + 16)
    rec = struct.pack("<QIIIB", 0, REG_ENTRY, new, group, 0)
    return b[:cat + 13] + struct.pack("<I", n + 1) + b[cat + 17:end] + rec + b[end:]


def add_branch(b: bytes, sid: int, like_tag: int, new_tag: int, old_child: int, new_child: int,
               new_ids: tuple[int, int]) -> bytes:
    """A selector with one more branch: a copy of the branch keyed by like_tag, keyed by new_tag and pointing at
    new_child instead of old_child, appended after the last branch. new_ids: the branch's and its tag list's ids."""
    count_off, end, brs = selector_branches(b, sid)
    br = next(x for x in brs if x["tag"] == like_tag)
    nb = bytearray(b[br["start"]:br["end"]])
    struct.pack_into("<Q", nb, 0, new_ids[0])
    struct.pack_into("<I", nb, br["tag_off"] - br["start"], new_tag)
    struct.pack_into("<Q", nb, 41, new_ids[1])  # the branch's tag list object
    nb = remap(bytes(nb), {old_child: new_child})
    out = b[:count_off] + struct.pack("<I", len(brs) + 1) + b[count_off + 4:end] + nb + b[end:]
    assert len(selector_branches(out, sid)[2]) == len(brs) + 1
    return out


def write_datafile(folder: str, eid: int, name: str, res: list[forge.Resource], level: int) -> str:
    data = forge.build_datafile(res, level=level)
    check = forge.parse_datafile(data)
    assert check.resources[0].id == eid and len(check.resources) == len(res)
    path = os.path.join(folder, f"{eid:016x}_{forge.safe_name(name)}.datafile")
    with open(path, "wb") as f:
        f.write(data)
    return path


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("list", help="items with category, price, flags")
    p.add_argument("game")
    p.add_argument("-g", "--grep")
    p.add_argument("--category", type=int)
    p.set_defaults(fn=cmd_list)
    p = sub.add_parser("show", help="one item decoded, who references it, its template chain")
    p.add_argument("game")
    p.add_argument("item")
    p.set_defaults(fn=cmd_show)
    p = sub.add_parser("costumes", help="the Clothing selectors' costume branches and the items that pick them")
    p.add_argument("game")
    p.set_defaults(fn=cmd_costumes)
    p = sub.add_parser("add-costume", help="clone a costume under new ids into unpacked/packed datafiles")
    p.add_argument("game")
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--like", help="the item to clone, e.g. COSTUME_MF_URBAN_OUTFIT_JAPAN_139")
    g.add_argument("--costume", help="or the costume selector to clone, e.g. STP_M_Costume_005 (the item is found)")
    p.add_argument("--name", required=True, help="the new item's resource name")
    p.add_argument("--label", help="short name in the cloned resources' names (default: from --name)")
    p.add_argument("--source-dir", action="append", default=[],
                   help="folder of unpacked entries (e.g. another mod's client/forge/DataPC_extra_chr) to clone from "
                        "instead of the game's copy, by entry id")
    p.add_argument("--keep-price", action="store_true", help="keep the source's price and flags (default: price 0, "
                   "default-item flag set)")
    p.add_argument("--title", help="the item's name in the game, e.g. \"Master Chief\": a new string id in every UI "
                   "language package (default: the source's name)")
    p.add_argument("--level", type=int, default=9, help="zstd level for the patched datafiles")
    p.add_argument("-o", "--out", required=True, help="output: <out>/DataPC/ and <out>/DataPC_extra_chr/")
    p.set_defaults(fn=cmd_add_costume)
    p = sub.add_parser("add-costumes", help="add several costumes in one go (shared resources patched once), from a "
                       "TOML file of [[costume]] tables")
    p.add_argument("game")
    p.add_argument("spec", help="[[costume]] name, like|costume, label, source, title, keep_price")
    p.add_argument("--level", type=int, default=9, help="zstd level for the patched datafiles")
    p.add_argument("-o", "--out", required=True, help="output: <out>/DataPC/ and <out>/DataPC_extra_chr/")
    p.set_defaults(fn=cmd_add_costumes)
    p = sub.add_parser("check", help="check add-costume(s)' output (a <name>.json it wrote) against the game; the "
                       "other costumes of the same run are taken into account")
    p.add_argument("game")
    p.add_argument("report", help="<out>/<name>.json")
    p.add_argument("--source-dir", action="append", default=[], help="as given to add-costume")
    p.set_defaults(fn=cmd_check)
    a = ap.parse_args(argv)
    return a.fn(a)


if __name__ == "__main__":
    sys.exit(main())
