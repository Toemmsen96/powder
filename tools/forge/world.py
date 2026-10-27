#!/usr/bin/env python3
"""Steep's world placement (AnvilNext cells): which objects a map places where, and edits to them.

A map archive (DataPC_STP_<Map>.forge + its _patch_NN) has one world resource (type 0xFBB63E47, e.g. STP_Alpes)
whose cell table points at the cell datafiles (`CellNNNNN_DataBlock`, type 0xAC2BBF68). A cell lists the objects
it places; each placed object is a resource of the cell datafile and carries its own world matrix, so an instance
is moved by rewriting that matrix, removed by dropping it from the list, and duplicated by copying it under fresh
ids. This tool lists cells and their objects, finds every placement of a prop, and writes a modified cell as an
unpacked datafile folder, as `forge.py extract` does, for an asset mod (`forge.py build --like <map>` packs it into
a _patch_03). The format, and what is confirmed vs guessed, is in WORLD.md next to this file.

Archives are only read (the newest copy of each entry, as the game loads it; archives a powder-wrap session put
there are left out). Python 3.10+, stdlib only (+ forge.py's zstd).
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import os
import re
import struct
import sys
from dataclasses import dataclass, field

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forge  # noqa: E402  (next to this file)

CELL_TYPE = 0xAC2BBF68       # CellNNNNN_DataBlock: the cell's object list (resource 0 of a cell datafile)
WORLD_TYPE = 0xFBB63E47      # the map's world resource (STP_Alpes, STP_WhiteRoom, ...)
GRID_CLASS = 0x0E2F4444      # in the world: u8 1, u32 levels, u32 slots, slots x cell link
SLOT_CLASS = 0x3435AD40      # one cell link: u64 0, u32 class, u8 0, u64 DataBlock id (0 = no cell)
ENTITY = 0x0984415E          # a placed object: world matrix + components (visual, physics, ...)
GROUP = 0x3F742D26           # a placed group (buildings, packed instances): matrix + components + child entities
PROXY = 0x1CBDE084           # a group's collision stand-in: shape link, world matrix, link back to the group
BATCH = 0x5730D30E           # `CellID=N`: a list of points (pos, scale, quaternion); what they place is unknown
BATCH_ITEM = 0xDB30CAB0
AABB_CLASS = 0x4AEC3476
PROP = 0x51DC6B80            # a prop template (LOD mesh ids + distances), embedded in every cell that places it
DATA_CLASSES = (BATCH, 0x88326C5A, 0xC29A81B3, 0x44F7C7A6)  # listed after the "placed" prefix (not entities)
KINDS = {ENTITY: "entity", GROUP: "group", PROXY: "proxy", BATCH: "batch", 0x88326C5A: "giprobes",
         0xC29A81B3: "rule?", 0x44F7C7A6: "layer?"}
LAYER_CLASS = 0xAFE5F9B4     # a world layer: settings (0x7DB083ED) and, for streamed layers, a cell table
SETTINGS_CLASS = 0x7DB083ED  # +4 u16 ?, +6 u16 cell size (m), +10 i32 min x, +14 i32 min y, +22 u32 cells per side
CELL_SIZE = 128.0            # the finest level's cell edge in metres when no settings say otherwise
NEW_ID_BASE = 0x1F000000000  # fresh ids for copies: above every id the shipped maps use (see WORLD.md)
CELL_NAME = re.compile(r"Cell(\d{5})_DataBlock$")

# Class hashes of nested objects (u64 id + u32 class), learned from the own-id blocks of ~6,300 sampled cells of
# all four maps (see WORLD.md). Used to find an instance's own sub-object ids, which a copy must renumber.
OBJECT_CLASSES = frozenset({
    0x4AEC3476, 0x5730D30E, 0x0984415E, 0xEC658D29, 0xDB30CAB0, 0x536E963B, 0x01437462, 0x88326C5A, 0x3CC7CD74,
    0x0E5A450A, 0x228F402A, 0x1C4B22AA, 0x3F742D26, 0x71FDA747, 0x9908895A, 0xE7F9A244, 0x1CBDE084, 0x9B52CA4D,
    0x78A13BF7, 0x49FCD7BF, 0xC29A81B3, 0x44F7C7A6, 0x7E42F87F, 0xB3E5916C, 0x33CE8E70, 0xD2BF93A6, 0x11598A66,
    0x2EE4DA30, 0xD2358CC7, 0xE8134060, 0x4E7C39C3, 0x0423BD15, 0x43F19E3B, 0x7313743E, 0x299309DE, 0xB0438131,
    0xCFC81A8A, 0xC91B143F, 0xADF1D5E6, 0xF60E530C, 0xCF153BBA, 0x81A7045D, 0x756DD6EC, 0x308358A4, 0x4404F7CC,
    0x3E7CFA88, 0xB8B08A89, 0x663832CD, 0xEACA451C, 0x3621488A, 0x1D566A63, 0x55AF1C3E, 0x4661AAEF, 0x22054903,
    0xDE0C5015, 0x7379D63D, 0xC50F7CAC, 0xC17F7BB7, 0xA251125B, 0xBD89BC9F, 0x262D59E5, 0x9A713616, 0x056125B6,
    0x554C614C, 0xAA8F96B6, 0xC5F33877, 0xFB1A684B, 0xB5504ABA, 0x2C2607FA, 0xE5FC91C8, 0x5BC80F05, 0x2ADB8BEF,
    0x2D7B69F6, 0x93D9BD33, 0x4FB33274, 0x1097F051, 0x97175472, 0x0A43616F, 0x00E1C5D1, 0x6D5AD517, 0x4E2800B4,
    0xE5C66AA6,
})
_CLASS_RE = re.compile(b"(?=(" + b"|".join(re.escape(struct.pack("<I", c)) for c in sorted(OBJECT_CLASSES)) + b"))")


# --- matrices (row-vector convention, as stored: rows 0-2 = the local axes in world space, row 3 = translation) ---

def mat_mul(a: list[float], b: list[float]) -> list[float]:
    return [sum(a[4 * r + k] * b[4 * k + c] for k in range(4)) for r in range(4) for c in range(4)]


def mat_rot_z(deg: float) -> list[float]:
    c, s = math.cos(math.radians(deg)), math.sin(math.radians(deg))
    return [c, s, 0, 0, -s, c, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]


def mat_translate(t) -> list[float]:
    return [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, t[0], t[1], t[2], 1]


def mat_scale(s: float) -> list[float]:
    return [s, 0, 0, 0, 0, s, 0, 0, 0, 0, s, 0, 0, 0, 0, 1]


def is_affine(m) -> bool:
    return abs(m[15] - 1) < 1e-5 and m[3] == m[7] == m[11] == 0 and all(math.isfinite(v) for v in m)


def decompose(m) -> tuple[tuple, tuple, tuple]:
    """Position, scale per axis, (yaw, pitch, roll) in degrees (Z-up; yaw about Z from +X)."""
    sx, sy, sz = (math.sqrt(sum(v * v for v in m[4 * r:4 * r + 3])) for r in range(3))
    x = [v / sx for v in m[0:3]] if sx else [1, 0, 0]
    z = [v / sz for v in m[8:11]] if sz else [0, 0, 1]
    yaw = math.degrees(math.atan2(x[1], x[0]))
    pitch = math.degrees(math.asin(max(-1.0, min(1.0, -x[2]))))
    roll = math.degrees(math.atan2(m[6] / sy if sy else 0, z[2]))
    return tuple(m[12:15]), (sx, sy, sz), (yaw, pitch, roll)


def read_mat(p: bytes, off: int) -> list[float]:
    return list(struct.unpack_from("<16f", p, off))


def write_mat(p: bytearray, off: int, m) -> None:
    struct.pack_into("<16f", p, off, *m)


# --- map stack, grid, cells -------------------------------------------------------------------------------------

def session_archives(folder: str) -> set[str]:
    try:
        with open(os.path.join(folder, ".powder-assets")) as f:
            return {line.strip() for line in f if line.strip()}
    except OSError:
        return set()


class Map:
    """A map archive and its patches; the newest copy of every entry, as the game loads it."""

    def __init__(self, base: str):
        base = re.sub(r"_patch_\d+\.forge$", ".forge", base)
        if not base.endswith(".forge"):
            base += ".forge"
        if not os.path.exists(base):
            sys.exit(f"{base}: no such archive")
        self.base = base
        self.name = os.path.basename(base)[:-len(".forge")]
        skip = session_archives(os.path.dirname(os.path.abspath(base)))
        self.archives = []
        for p in forge.patch_stack(base):
            if not os.path.exists(p):
                continue
            if os.path.basename(p) in skip:
                print(f"  ({os.path.basename(p)} is a powder-wrap session's, left out)", file=sys.stderr)
                continue
            self.archives.append(forge.Forge(p))
        self.entries: dict[int, tuple[forge.Forge, forge.Entry]] = {}
        for fg in self.archives:  # lowest first: later archives override
            for e in fg.entries:
                if e.id not in (forge.META_ID, forge.PREFETCH_ID):
                    self.entries[e.id] = (fg, e)
        self.world_entry = next(((fg, e) for fg, e in self.entries.values() if e.type == WORLD_TYPE), None)
        if not self.world_entry:
            sys.exit(f"{self.name}: no world resource (type {WORLD_TYPE:#010x})")
        fg, e = self.world_entry
        self.layers = Grid.parse_all(forge.parse_datafile(fg.read(e)).resources[0].payload)
        self.grid = self.layers[0]  # the geometry (all layers of a map have the same levels; C on the four maps)
        # (layer, index) -> the cell datafile the world's table points at (two layers can share an index and a name)
        self.cells: dict[tuple[int, int], tuple[forge.Forge, forge.Entry]] = {}
        for g in self.layers:
            for k, eid in g.slots.items():
                if eid in self.entries:
                    self.cells[(g.layer, k)] = self.entries[eid]
        listed = {e.id for _, e in self.cells.values()}
        self.unreferenced = sorted(e.name for _, e in self.entries.values()
                                   if e.type == CELL_TYPE and CELL_NAME.match(e.name) and e.id not in listed)
        self._blobs = None

    def prefetch_blob(self, eid: int) -> bytes | None:
        """The entry's prefetch blob (its dependency list) from the newest archive that has one."""
        if self._blobs is None:
            self._blobs = {}
            for fg in self.archives:
                self._blobs.update(forge.read_prefetch(fg))
        return self._blobs.get(eid)

    def key(self, index: int, layer: int | None = None) -> tuple[int, int]:
        """(layer, index) of a cell; the layer may be left out when only one layer has that index."""
        if layer is not None:
            if (layer, index) not in self.cells:
                sys.exit(f"{self.name}: no cell {index} in layer {layer}")
            return layer, index
        have = [k for k in self.cells if k[1] == index]
        if len(have) != 1:
            sys.exit(f"{self.name}: cell {index} is in " + (f"layers {', '.join(str(k[0]) for k in have)}: pass --layer"
                                                             if have else "no layer"))
        return have[0]

    def cell(self, index: int, layer: int | None = None) -> Cell:
        """A cell as the archives hold it; with `keep_cells` (edit, apply) parsed once (edits work on copies)."""
        key = self.key(index, layer)
        cache = self.__dict__.setdefault("_cell_cache", {})
        if key not in cache:
            fg, e = self.cells[key]
            c = Cell(index, os.path.basename(fg.path), e, forge.parse_datafile(fg.read(e)).resources, key[0])
            if not getattr(self, "keep_cells", False):
                return c
            cache[key] = c
        return cache[key]


@dataclass
class Grid:
    """One cell table of the world (a layer): a quadtree stored finest level first, row-major, 128 m cells at level
    0 (C)."""
    layer: int
    id: int
    levels: int
    slots: dict[int, int]  # cell index -> DataBlock id (filled slots only)
    cell: float = CELL_SIZE  # level 0 cell edge (m); level k cells are cell * 2^k
    min_x: float = 0.0
    min_y: float = 0.0
    from_settings: bool = False  # cell size and origin read from the layer settings (else assumed)

    @classmethod
    def parse_all(cls, p: bytes) -> list[Grid]:
        out = []
        for m in re.finditer(re.escape(struct.pack("<I", GRID_CLASS)), p):
            i = m.start()
            gid = struct.unpack_from("<Q", p, i - 8)[0]
            one, levels, n = struct.unpack_from("<BII", p, i + 4)
            if one != 1 or not 1 <= levels <= 12 or n != (4 ** levels - 1) // 3:
                continue
            slots = {}
            for k in range(n):
                oid, c, z, link = struct.unpack_from("<QIBQ", p, i + 13 + 21 * k)
                if c != SLOT_CLASS or oid or z:
                    raise ValueError(f"cell table {gid:#x} slot {k}: unexpected {c:#010x}")
                if link:
                    slots[k] = link
            g = cls(len(out), gid, levels, slots)
            g.min_x = g.min_y = -CELL_SIZE * 2 ** (levels - 1) / 2
            st = p.rfind(struct.pack("<I", SETTINGS_CLASS), 0, i)  # the layer's settings, just before its table
            if st > 0 and i - st < 64:
                size, x0, y0 = struct.unpack_from("<H", p, st + 6)[0], *struct.unpack_from("<2i", p, st + 10)
                side = struct.unpack_from("<I", p, st + 22)[0]
                if size and side == 2 ** (levels - 1):
                    g.cell, g.min_x, g.min_y, g.from_settings = float(size), float(x0), float(y0), True
            out.append(g)
        if not out:
            raise ValueError("no cell table (class 0x0E2F4444) in the world resource")
        return out

    @property
    def extent(self) -> float:
        return self.cell * 2 ** (self.levels - 1)

    def side(self, level: int) -> int:
        return 2 ** (self.levels - 1 - level)

    def first(self, level: int) -> int:
        return sum(self.side(k) ** 2 for k in range(level))

    def locate(self, index: int) -> tuple[int, int, int]:
        """Cell index -> (level, column, row)."""
        for level in range(self.levels):
            n = self.side(level) ** 2
            if index < n:
                return level, index % self.side(level), index // self.side(level)
            index -= n
        raise ValueError("cell index past the last level")

    def rect(self, index: int) -> tuple[float, float, float]:
        level, col, row = self.locate(index)
        size = self.cell * 2 ** level
        return self.min_x + col * size, self.min_y + row * size, size

    def at(self, x: float, y: float, level: int) -> int | None:
        size, side = self.cell * 2 ** level, self.side(level)
        col, row = math.floor((x - self.min_x) / size), math.floor((y - self.min_y) / size)
        if 0 <= col < side and 0 <= row < side:
            return self.first(level) + row * side + col
        return None

    def contains(self, index: int, x: float, y: float, margin: float = 0.0) -> bool:
        x0, y0, size = self.rect(index)
        return x0 - margin <= x < x0 + size + margin and y0 - margin <= y < y0 + size + margin


# --- cells ---------------------------------------------------------------------------------------------------------

@dataclass
class DataBlock:
    """CellNNNNN_DataBlock (C): u64 id, u32 type, u8 1, u32 n, n x (u8 1, u8 0, u64 object id), u32 placed (how
    many listed objects are entities/groups/proxies; they come first), u32 cell index, u32 0."""
    id: int
    ids: list[int]
    placed: int
    index: int

    @classmethod
    def parse(cls, p: bytes) -> DataBlock:
        rid, typ, one, n = struct.unpack_from("<QIBI", p)
        if typ != CELL_TYPE or one != 1:
            raise ValueError(f"not a cell DataBlock ({typ:#010x}, {one})")
        ids = []
        for i in range(n):
            a, b, oid = struct.unpack_from("<BBQ", p, 17 + 10 * i)
            if (a, b) != (1, 0):
                raise ValueError(f"DataBlock record {i}: {a}, {b}")
            ids.append(oid)
        placed, index, zero = struct.unpack_from("<3I", p, 17 + 10 * n)
        if zero or len(p) != 17 + 10 * n + 12:
            raise ValueError("DataBlock tail")
        return cls(rid, ids, placed, index)

    def build(self) -> bytes:
        out = bytearray(struct.pack("<QIBI", self.id, CELL_TYPE, 1, len(self.ids)))
        for oid in self.ids:
            out += struct.pack("<BBQ", 1, 0, oid)
        out += struct.pack("<3I", self.placed, self.index, 0)
        return bytes(out)


@dataclass
class Placed:
    """A listed object of a cell, decoded as far as its class allows."""
    res: forge.Resource
    kind: str
    matrix_at: int | None = None                   # offset of the world matrix in the payload
    props: list[str] = field(default_factory=list)  # prop templates it refers to (names)
    children: list[int] = field(default_factory=list)  # nested entities' matrix offsets (groups)
    child_ids: list[int] = field(default_factory=list)  # ... and their ids (proxies may stand in for them)
    targets: list[int] = field(default_factory=list)  # proxies: the entities/groups they stand in for
    items: int = 0                                 # batches: number of points

    @property
    def matrix(self) -> list[float] | None:
        return read_mat(self.res.payload, self.matrix_at) if self.matrix_at is not None else None


def object_headers(p: bytes, exclude: set[int]) -> list[tuple[int, int, int]]:
    """(offset, id, class) of nested objects with an id of their own: an id-like u64 (2^32..2^42) followed by a
    known object class, the id not being a resource of the datafile (those are links)."""
    out = []
    for m in _CLASS_RE.finditer(p, 8):
        o = m.start() - 8
        if p[o + 5] > 3 or p[o + 6] or p[o + 7] or not (p[o + 4] or p[o + 5]) or not any(p[o:o + 4]):
            continue
        v = struct.unpack_from("<Q", p, o)[0]
        if v not in exclude:
            out.append((o, v, struct.unpack_from("<I", p, m.start())[0]))
    return out


class Cell:
    def __init__(self, index: int, archive: str, entry: forge.Entry, resources: list[forge.Resource], layer: int = 0):
        self.index, self.archive, self.entry, self.layer = index, archive, entry, layer
        self.resources = resources
        self.block = DataBlock.parse(resources[0].payload)
        self.by_id = {r.id: r for r in resources}
        self.missing = [i for i in self.block.ids if i not in self.by_id]  # listed, but in no datafile (C: rare)

    @property
    def name(self) -> str:
        return self.entry.name

    def objects(self) -> list[Placed]:
        return [self.placed_object(i) for i in self.block.ids]

    def placed_object(self, i: int) -> Placed:
        """Listed object `i`, decoded once per cell (a cell's resources do not change; edits copy them)."""
        cache = self.__dict__.setdefault("_decoded", {})
        if i not in cache:
            cache[i] = self.decode(self.by_id[i]) if i in self.by_id else Placed(forge.Resource(i, 0, "", b""),
                                                                                  "missing")
        return cache[i]

    def proxies(self) -> list[Placed]:
        """The listed proxies only (what following a moved object needs: no prop/child scans of the others)."""
        return [self.placed_object(i) for i in self.block.ids if i in self.by_id and self.by_id[i].type == PROXY]

    def prop_names(self, p: bytes, limit: int = 8) -> list[str]:
        props = {r.id: r.name for r in self.resources if r.type == PROP}
        out = []
        for m in re.finditer(b"(?=([\\s\\S]{5}[\\x00-\\x03]\\x00\\x00))", p):
            v = struct.unpack_from("<Q", p, m.start())[0]
            if v in props and props[v] not in out:
                out.append(props[v])
                if len(out) >= limit:
                    break
        return out

    def decode(self, r: forge.Resource) -> Placed:
        p = r.payload
        kind = KINDS.get(r.type, f"{r.type:#010x}")
        if r.type in (ENTITY, GROUP):
            pl = Placed(r, kind, 14, self.prop_names(p))
            if r.type == GROUP:
                rids = set(self.by_id)
                kids = [(o, v) for o, v, c in object_headers(p, rids) if c == ENTITY]
                pl.children, pl.child_ids = [o + 14 for o, _ in kids], [v for _, v in kids]
            return pl
        if r.type == PROXY:  # ... f32[16] world matrix, f32 1, u32 n, n x (u8 0, u64 target) at the end (C)
            for n in range(1, 9):
                off = len(p) - 72 - 9 * n
                if off >= 12 and struct.unpack_from("<I", p, off + 68)[0] == n and is_affine(read_mat(p, off)):
                    return Placed(r, kind, off, targets=[struct.unpack_from("<Q", p, off + 73 + 9 * k)[0]
                                                         for k in range(n)])
        if r.type == BATCH and len(p) >= 57:
            return Placed(r, kind, items=struct.unpack_from("<I", p, 53)[0])
        return Placed(r, kind)

    def find(self, spec: str) -> Placed:
        """A listed object by id (0x...) or exact name."""
        if re.fullmatch(r"0x[0-9a-fA-F]+", spec):
            hit = [i for i in self.block.ids if i == int(spec, 16)]
        else:
            hit = [i for i in self.block.ids if (self.by_id[i].name if i in self.by_id else "") == spec]
        hit = [self.placed_object(i) for i in hit]
        if len(hit) != 1:
            sys.exit(f"{self.name}: {len(hit)} listed objects match {spec!r}")
        return hit[0]


def batch_items(p: bytes):
    """A batch's points (C layout): (id, (x, y, z), scale, quaternion x, y, z, w)."""
    n = struct.unpack_from("<I", p, 53)[0]
    for k in range(n):
        o = 57 + 44 * k
        oid, c = struct.unpack_from("<QI", p, o)
        v = struct.unpack_from("<8f", p, o + 12)
        yield oid, v[0:3], v[3], v[4:8]


# --- edits ---------------------------------------------------------------------------------------------------------

def own_ids(r: forge.Resource, resource_ids: set[int]) -> list[int]:
    """The ids an instance owns: its own and its nested objects' (in the order first seen)."""
    out = [r.id]
    for _, v, _ in object_headers(r.payload, resource_ids - {r.id}):
        if v not in out:
            out.append(v)
    return out


def suspicious_ids(r: forge.Resource, own: list[int], resource_ids: set[int]) -> list[int]:
    """Id-like values right next to the instance's id block that are neither own ids nor links to resources: maybe
    nested objects of a class OBJECT_CLASSES lacks (a copy would then share them with the original)."""
    known, out = set(own) | resource_ids, []
    p = r.payload
    for o in range(len(p) - 7):
        if p[o + 7] or p[o + 6] or p[o + 5] > 3:
            continue
        v = struct.unpack_from("<Q", p, o)[0]
        if v not in known and r.id < v < r.id + 0x400 and v not in out:
            out.append(v)
    return out


def transform_object(pl: Placed, delta: list[float], rids: set[int]) -> bytes:
    """`pl`'s payload with `delta` (world space) applied to its matrix and its children's. A turn or scale also
    turns its bounding boxes (0x4AEC3476: min/max around the origin, axis-aligned in world space; G), conservatively."""
    p = bytearray(pl.res.payload)
    for off in [pl.matrix_at] + pl.children:
        write_mat(p, off, mat_mul(read_mat(p, off), delta))
    lin = delta[0:3] + delta[4:7] + delta[8:11]
    if any(abs(v - w) > 1e-7 for v, w in zip(lin, [1, 0, 0, 0, 1, 0, 0, 0, 1])):
        for o, _, c in object_headers(bytes(p), rids - {pl.res.id}):
            if c == AABB_CLASS and o + 36 <= len(p):
                lo, hi = struct.unpack_from("<3f", p, o + 12), struct.unpack_from("<3f", p, o + 24)
                corners = [[(hi if (k >> i) & 1 else lo)[i] for i in range(3)] for k in range(8)]
                moved = [[sum(c3[i] * lin[3 * i + j] for i in range(3)) for j in range(3)] for c3 in corners]
                struct.pack_into("<6f", p, o + 12, *[min(v[j] for v in moved) for j in range(3)],
                                 *[max(v[j] for v in moved) for j in range(3)])
    return bytes(p)


def proxies_of(cell: Cell, target: int) -> list[Placed]:
    return [o for o in cell.proxies() if o.kind == "proxy" and target in o.targets]


def merge_prefetch(own: bytes | None, extra: bytes | None, wanted: set[int]) -> bytes | None:
    """A cell's dependency list (u16 0, u16 k, k x (u64 entry id, 3 bytes); README.md) plus the records of another's
    that name an entry in `wanted` (G: the copied object's meshes and shapes are what it needs)."""
    recs = {}
    for blob, keep in ((own, None), (extra, wanted)):
        if not blob or len(blob) < 4:
            continue
        _, k = struct.unpack_from("<HH", blob)
        for j in range(k):
            rid = struct.unpack_from("<Q", blob, 4 + 11 * j)[0]
            if keep is None or rid in keep:
                recs.setdefault(rid, blob[4 + 11 * j:15 + 11 * j])
    head = struct.unpack_from("<H", own)[0] if own and len(own) >= 4 else 0
    return struct.pack("<HH", head, len(recs)) + b"".join(recs.values()) if recs else own


def linked_ids(payloads) -> set[int]:
    """Every id-like u64 in these payloads (links to entries are among them)."""
    out = set()
    for p in payloads:
        for m in re.finditer(b"(?=([\\s\\S]{5}[\\x00-\\x03]\\x00\\x00))", p):
            out.add(struct.unpack_from("<Q", p, m.start())[0])
    return out


def dependencies(src: Cell, r: forge.Resource, have: set[int]) -> list[forge.Resource]:
    """Resources of `src`'s datafile that `r` links to (transitively) and that `have` lacks."""
    out, todo, seen = [], [r], {r.id}
    while todo:
        p = todo.pop().payload
        for m in re.finditer(b"(?=([\\s\\S]{5}[\\x00-\\x03]\\x00\\x00))", p):
            v = struct.unpack_from("<Q", p, m.start())[0]
            dep = src.by_id.get(v)
            if dep is not None and v not in seen and dep.type != CELL_TYPE and v not in src.block.ids:
                seen.add(v)
                if v not in have:
                    out.append(dep)
                todo.append(dep)
    return out


def copy_instance(src: Cell, pl: Placed, new_base: int, warn) -> tuple[forge.Resource, dict[int, int]]:
    """A copy of an entity under fresh ids (its own and its nested objects')."""
    rids = set(src.by_id)
    own = own_ids(pl.res, rids)
    odd = suspicious_ids(pl.res, own, rids)
    if odd:
        warn(f"{pl.res.name}: {len(odd)} id-like value(s) next to its id block are not renumbered: "
             + ", ".join(hex(v) for v in odd[:6]))
    mapping = {old: new_base + k for k, old in enumerate(own)}
    p = pl.res.payload
    for old, new in mapping.items():
        p = p.replace(struct.pack("<Q", old), struct.pack("<Q", new))
    stem = re.sub(r"_\d{3}$", "", pl.res.name) or "Copy"
    r = forge.Resource(new_base, pl.res.type, f"{stem}_PWD{(new_base >> 12) & 0xFF:03d}", p, pl.res.flag,
                       pl.res.pre, pl.res.extra)
    return r, mapping


def parse_vec(s: str, n: int, name: str) -> list[float]:
    try:
        v = [float(x) for x in s.split(",")]
    except ValueError:
        sys.exit(f"{name}: {s!r} is not {n} comma-separated numbers")
    if len(v) not in (n, n + 1):
        sys.exit(f"{name}: {s!r} needs {n} numbers" + (f" (or {n + 1} with a yaw)" if name == "--copy" else ""))
    return v


class CellEdit:
    """A cell being changed: its resources in datafile order, its object list and placed count, a log."""

    def __init__(self, cell: Cell):
        self.cell = cell
        self.res = {r.id: r for r in cell.resources}
        self.order = [r.id for r in cell.resources]
        self.ids = list(cell.block.ids)
        self.placed = cell.block.placed
        self.log: list[str] = []
        self.more: dict = {}

    def set_payload(self, r: forge.Resource, payload: bytes) -> None:
        self.res[r.id] = forge.Resource(r.id, r.type, r.name, payload, r.flag, r.pre, r.extra)

    def drop(self, rid: int) -> None:
        if rid in self.ids:
            self.placed -= self.ids.index(rid) < self.placed
            self.ids.remove(rid)
        self.order.remove(rid)
        del self.res[rid]

    def add_placed(self, r: forge.Resource) -> None:
        """A new placed object: listed at the end of the placed prefix (ids there ascend; new ids are the largest),
        its resource after the last placed object's."""
        after = self.ids[self.placed - 1] if self.placed else self.cell.block.id
        self.order.insert(self.order.index(after) + 1, r.id)
        self.ids.insert(self.placed, r.id)
        self.placed += 1
        self.res[r.id] = r

    def add_dependency(self, r: forge.Resource) -> None:
        self.order.append(r.id)
        self.res[r.id] = r

    def resources(self) -> list[forge.Resource]:
        r0 = self.cell.resources[0]
        block = DataBlock(self.cell.block.id, self.ids, self.placed, self.cell.block.index)
        self.res[r0.id] = forge.Resource(r0.id, r0.type, r0.name, block.build(), r0.flag, r0.pre, r0.extra)
        return [self.res[i] for i in self.order]


def edit_cell(mp: Map, cell: Cell, a, warn) -> tuple[CellEdit, dict[int, list[float] | None]]:
    """Apply --remove/--move/--offset/--yaw/--scale/--copy to `cell`. Returns the edit and what happened to which
    object ({id: world-space delta, or None if removed}) so that proxies in other cells can follow."""
    ce = CellEdit(cell)
    rids = set(cell.by_id)
    targets: dict[int, list[float] | None] = {}

    for spec in a.remove or []:
        pl = cell.find(spec)
        own = [x.res.id for t in [pl.res.id] + pl.child_ids for x in proxies_of(cell, t)]
        for rid in [pl.res.id] + own:
            ce.drop(rid)
        for t in [pl.res.id] + pl.child_ids:
            targets[t] = None
        ce.log.append(f"removed {pl.res.name or hex(pl.res.id)}" + (f" (+{len(own)} proxy)" if own else ""))

    moves: dict[int, tuple[Placed, list[float]]] = {}  # object id -> (object, world-space delta so far)

    def add_delta(spec, make):
        pl = cell.find(spec)
        if pl.matrix_at is None:
            sys.exit(f"{spec}: has no matrix (kind {pl.kind})")
        if pl.res.id in targets and targets[pl.res.id] is None:
            sys.exit(f"{spec}: removed and moved")
        d = moves.get(pl.res.id, (pl, mat_translate((0, 0, 0))))[1]
        moves[pl.res.id] = (pl, mat_mul(d, make(mat_mul(pl.matrix, d))))  # from where earlier operations put it

    for spec, v in a.move or []:
        t = parse_vec(v, 3, "--move")
        add_delta(spec, lambda cur: mat_translate([t[k] - cur[12 + k] for k in range(3)]))
    for spec, v in a.offset or []:
        t = parse_vec(v, 3, "--offset")
        add_delta(spec, lambda cur: mat_translate(t[:3]))
    for spec, v in a.yaw or []:
        add_delta(spec, lambda cur: about(cur, mat_rot_z(float(v))))
    for spec, v in a.scale or []:
        add_delta(spec, lambda cur: about(cur, mat_scale(float(v))))
    for pl, d in moves.values():
        before = pl.matrix
        ce.set_payload(pl.res, transform_object(pl, d, rids))
        for t in [pl.res.id] + pl.child_ids:  # proxies stand in for objects and for groups' child entities (C)
            for px in proxies_of(cell, t):
                ce.set_payload(px.res, transform_object(px, d, rids))
            targets[t] = d
        after = mat_mul(before, d)
        ce.log.append(f"moved {pl.res.name}: {fmt_pos(before)} -> {fmt_pos(after)}"
                      + (f" (+{len(pl.children)} child matrices)" if pl.children else ""))
        check_inside(mp, cell, after, pl.res.name, warn)

    src = cell if a.source is None else mp.cell(a.source, a.layer)
    for k, (spec, v) in enumerate(a.copy or []):
        vals = parse_vec(v, 3, "--copy")
        new_base = a.id + (k << 12) if a.id else NEW_ID_BASE + (cell.index << 20) + ((a.copy_no + k) << 12)
        add_copy(mp, ce, src, src.find(spec), vals[:3], vals[3] if len(vals) == 4 else 0.0, 1.0, new_base, warn)
    finish_prefetch(ce)
    return ce, targets


def about(cur, m):
    """`m` (a turn or scale) about the origin of the object whose matrix is `cur`."""
    o = cur[12:15]
    return mat_mul(mat_mul(mat_translate([-c for c in o]), m), mat_translate(o))


def add_copy(mp: Map, ce: CellEdit, src: Cell, pl: Placed, pos, yaw: float, scale: float, new_base: int,
             warn) -> forge.Resource:
    """A copy of the entity `pl` (of cell `src`) placed in `ce`'s cell at `pos`, turned by `yaw` degrees and scaled
    by `scale` about its origin, under the fresh id block `new_base`. From another cell, the entity's dependencies
    and prefetch records come along."""
    cell = ce.cell
    if pl.kind != "entity":
        sys.exit(f"{pl.res.name or hex(pl.res.id)}: only entities (0x0984415E) can be copied, this is a {pl.kind}")
    clash = next((i for i in range(new_base, new_base + 0x1000) if i in mp.entries or i in ce.res), None)
    if clash is not None:
        sys.exit(f"--copy: the new id block {new_base:#x} clashes with {clash:#x}; pass --id or --copy-no")
    r, mapping = copy_instance(src, pl, new_base, warn)
    cur = read_mat(r.payload, 14)
    d = mat_translate([pos[i] - cur[12 + i] for i in range(3)])
    if yaw:
        d = mat_mul(d, about(mat_mul(cur, d), mat_rot_z(yaw)))
    if scale != 1.0:
        d = mat_mul(d, about(mat_mul(cur, d), mat_scale(scale)))
    moved = Placed(r, "entity", 14)
    r = forge.Resource(r.id, r.type, r.name, transform_object(moved, d, set(src.by_id) | {r.id}), r.flag, r.pre,
                       r.extra)
    deps = dependencies(src, pl.res, set(ce.res)) if src.entry.id != cell.entry.id else []
    for dep in deps:
        ce.add_dependency(dep)
    ce.add_placed(r)
    at = read_mat(r.payload, 14)
    ce.log.append(f"copied {pl.res.name} as {r.name} ({r.id:#x}, {len(mapping)} ids renumbered) at {fmt_pos(at)}"
                  + (f", +{len(deps)} dependencies from cell {src.index}" if deps else ""))
    check_inside(mp, cell, at, r.name, warn)
    if src.entry.id != cell.entry.id:
        wanted = linked_ids([pl.res.payload] + [x.payload for x in dependencies(src, pl.res, set())])
        if "prefetch" in ce.more:  # finished by an earlier step
            ce.more["prefetch_raw"] = bytes.fromhex(ce.more.pop("prefetch"))
        blob = merge_prefetch(ce.more.get("prefetch_raw", mp.prefetch_blob(cell.entry.id)),
                              mp.prefetch_blob(src.entry.id), wanted)
        if blob is not None:
            ce.more["prefetch_raw"] = blob
    return r


def finish_prefetch(ce: CellEdit) -> None:
    if "prefetch_raw" in ce.more:
        ce.more["prefetch"] = ce.more.pop("prefetch_raw").hex()


def neighbours(g: Grid, index: int, radius: int) -> list[int]:
    level, col, row = g.locate(index)
    side, first = g.side(level), g.first(level)
    return [first + r * side + c for r in range(row - radius, row + radius + 1)
            for c in range(col - radius, col + radius + 1)
            if 0 <= r < side and 0 <= c < side and (r, c) != (row, col)]


def edit_proxies(mp: Map, cell: Cell, targets: dict[int, list[float] | None], radius: int,
                 warn, edits: dict[tuple[int, int], CellEdit] | None = None) -> list[CellEdit]:
    """The proxies (0x1CBDE084) standing in for moved/removed objects in the cells around `cell` (same layer and
    level, and the coarser cells over them): moved along or dropped. With `edits` ((layer, index) -> edit), a cell
    already being edited takes the changes, and new edits are added to it."""
    out = []
    g = mp.grid
    near = neighbours(g, cell.index, radius)
    x0, y0, size = g.rect(cell.index)
    lo, hi = (x0 - radius * size, y0 - radius * size), (x0 + (radius + 1) * size, y0 + (radius + 1) * size)
    for lvl in range(g.locate(cell.index)[0] + 1, g.levels):  # coarser cells overlapping the neighbourhood
        for idx in range(g.first(lvl), g.first(lvl) + g.side(lvl) ** 2):
            cx, cy, cs = g.rect(idx)
            if cx < hi[0] and cx + cs > lo[0] and cy < hi[1] and cy + cs > lo[1]:
                near.append(idx)
    for idx in near:
        if (cell.layer, idx) not in mp.cells:
            continue
        c = mp.cell(idx, cell.layer)
        ce = edits.get((cell.layer, idx)) if edits is not None else None
        for o in c.proxies():
            hit = [t for t in o.targets if t in targets]
            if o.kind != "proxy" or not hit or ce is not None and o.res.id not in ce.res:
                continue
            if len(hit) < len(o.targets) or any(targets[t] != targets[hit[0]] for t in hit):
                warn(f"cell {idx}: proxy {o.res.id:#x} stands in for {len(o.targets)} objects that do not all move "
                     "alike; left as it is")
                continue
            ce = ce or CellEdit(c)
            d = targets[hit[0]]
            if d is None:
                ce.drop(o.res.id)
                ce.log.append(f"removed the proxy {o.res.id:#x} of {hit[0]:#x}")
            else:
                ce.set_payload(o.res, transform_object(o, d, set(c.by_id)))
                ce.log.append(f"moved the proxy {o.res.id:#x} of {hit[0]:#x} along: {fmt_pos(o.matrix)} -> "
                              f"{fmt_pos(mat_mul(o.matrix, d))}")
        if ce and ce not in out:
            out.append(ce)
            if edits is not None:
                edits[(cell.layer, idx)] = ce
    return out


def check_inside(mp: Map, cell: Cell, m, name: str, warn) -> None:
    x, y = m[12], m[13]
    if not mp.grid.contains(cell.index, x, y):
        best = next((i for lvl in range(mp.grid.levels) if (i := mp.grid.at(x, y, lvl)) is not None
                     and (cell.layer, i) in mp.cells), None)
        warn(f"{name}: ({x:.1f}, {y:.1f}) is outside cell {cell.index}'s square; streaming may not load it there"
             + (f" (cell {best} covers it)" if best is not None else ""))


def fmt_pos(m) -> str:
    return f"({m[12]:.2f}, {m[13]:.2f}, {m[14]:.2f})"


# --- commands ------------------------------------------------------------------------------------------------------

def cmd_grid(a):
    mp = Map(a.map)
    g = mp.grid
    fg, e = mp.world_entry
    print(f"{mp.name}: world {e.name} ({e.id:#x}, {os.path.basename(fg.path)}), {len(mp.layers)} cell table(s) "
          f"(layers), {g.levels} levels, {len(mp.cells)} cells")
    print(f"  extent x {g.min_x:.0f}..{g.min_x + g.extent:.0f}, y {g.min_y:.0f}..{g.min_y + g.extent:.0f} m "
          f"(cells of {g.cell:.0f} m at level 0)")
    for lay in mp.layers:
        print(f"  layer {lay.layer} (table {lay.id:#x}): {len(lay.slots)} of {(4 ** lay.levels - 1) // 3} slots "
              f"filled, origin ({lay.min_x:.0f}, {lay.min_y:.0f}), {lay.cell:.0f} m cells"
              + (" (layer settings)" if lay.from_settings else " (assumed: no layer settings found)"))
        for lvl in range(lay.levels):
            first, n = lay.first(lvl), lay.side(lvl) ** 2
            filled = sum(1 for k in lay.slots if first <= k < first + n)
            print(f"    level {lvl}: {lay.side(lvl)}x{lay.side(lvl)} cells of {lay.cell * 2 ** lvl:.0f} m, "
                  f"indices {first}..{first + n - 1}, {filled} filled")
        missing = [k for k, eid in lay.slots.items() if eid not in mp.entries]
        if missing:
            print(f"    {len(missing)} slot(s) point at no entry of the stack, e.g. {missing[:5]}")
    if mp.unreferenced:
        print(f"  {len(mp.unreferenced)} CellNNNNN_DataBlock entries no table points at, e.g. "
              f"{', '.join(mp.unreferenced[:3])}")
    other = sorted(e.name for _, e in mp.entries.values() if e.type == CELL_TYPE and not CELL_NAME.match(e.name))
    if other:
        print(f"  {len(other)} other DataBlocks (not in a cell table), e.g. {', '.join(other[:3])}")


def cmd_cells(a):
    mp = Map(a.map)
    g = mp.grid
    want = None
    if a.at:
        x, y = parse_vec(a.at, 2, "--at")[:2]
        want = [i for lvl in range(g.levels) if (i := g.at(x, y, lvl)) is not None]
    rows = []
    for layer, idx in sorted(mp.cells):
        lvl, col, row = g.locate(idx)
        if (a.level is not None and lvl != a.level or want is not None and idx not in want
                or a.layer is not None and layer != a.layer):
            continue
        fg, e = mp.cells[(layer, idx)]
        x0, y0, size = g.rect(idx)
        rows.append((layer, idx, lvl, col, row, x0, y0, size, e, os.path.basename(fg.path)))
    for layer, idx, lvl, col, row, x0, y0, size, e, arc in rows[:a.max]:
        line = (f"L{layer} Cell{idx:05d}  level {lvl}  col {col:3} row {row:3}  x {x0:8.0f}..{x0 + size:<8.0f} "
                f"y {y0:8.0f}..{y0 + size:<8.0f} {e.size:9} B  {arc}")
        if a.count:
            kinds = collections.Counter(o.kind for o in mp.cell(idx, layer).objects())
            line += "  " + ", ".join(f"{v} {k}" for k, v in sorted(kinds.items()))
        print(line)
    if len(rows) > a.max:
        print(f"... {len(rows) - a.max} more (--max)")
    if want is not None:
        print(f"cells covering ({x:.1f}, {y:.1f}): " + ", ".join(
            f"{i} (level {g.locate(i)[0]}, " + (", ".join(f"L{k}" for k in range(len(mp.layers)) if (k, i) in mp.cells)
                                                or "none") + ")" for i in want))


def describe(pl: Placed) -> str:
    r = pl.res
    head = f"{r.id:#014x}  {pl.kind:8} {r.name or '(no name)':48}"
    if pl.matrix_at is not None:
        m = pl.matrix
        if not is_affine(m):
            return head + " (matrix not affine?)"
        (x, y, z), s, (yaw, pitch, roll) = decompose(m)
        scale = f"{s[0]:.2f}" if max(s) - min(s) < 1e-3 else "/".join(f"{v:.2f}" for v in s)
        line = (f"{head} pos {x:9.2f} {y:9.2f} {z:8.2f}  yaw {yaw:7.1f} pitch {pitch:5.1f} roll {roll:5.1f}  "
                f"scale {scale}")
        if pl.props:
            line += f"  prop {pl.props[0]}" + (f" +{len(pl.props) - 1}" if len(pl.props) > 1 else "")
        if pl.children:
            line += f"  children {len(pl.children)}"
        if pl.targets:
            line += "  for " + ", ".join(f"{t:#x}" for t in pl.targets)
        return line
    if pl.kind == "batch":
        p = r.payload
        bb = struct.unpack_from("<6f", p, 25)
        return (f"{head} {pl.items} points in x {bb[0]:.0f}..{bb[3]:.0f} y {bb[1]:.0f}..{bb[4]:.0f} "
                f"z {bb[2]:.0f}..{bb[5]:.0f}")
    return f"{head} {len(r.payload)} bytes"


def load_cell(mp: Map, index: int, layer: int | None, folder: str | None) -> Cell:
    """A cell from the map, or (folder) from an unpacked datafile folder such as `edit` writes."""
    if not folder:
        return mp.cell(index, layer)
    m, res = forge.load_unpacked(folder)
    e = forge.Entry(0, int(m["entry"], 16), 0, 0, 0, 0, int(m["type"], 16), 0, m["name"], ())
    return Cell(index, folder, e, res, m.get("world", {}).get("layer", layer or 0))


def cmd_show(a):
    mp = Map(a.map)
    c = load_cell(mp, a.cell, a.layer, a.dir)
    x0, y0, size = mp.grid.rect(c.index)
    lvl = mp.grid.locate(c.index)[0]
    print(f"L{c.layer} {c.name} ({c.entry.id:#x}, {c.archive}): level {lvl}, x {x0:.0f}..{x0 + size:.0f}, "
          f"y {y0:.0f}..{y0 + size:.0f}; {len(c.block.ids)} listed objects ({c.block.placed} placed), "
          f"{len(c.resources)} resources")
    objs = c.objects()
    if a.json:
        for o in objs:
            d = {"id": f"{o.res.id:#x}", "kind": o.kind, "class": f"{o.res.type:#010x}", "name": o.res.name}
            if o.matrix_at is not None:
                d["matrix"] = [round(v, 6) for v in o.matrix]
                d["props"] = o.props
            if o.targets:
                d["for"] = [f"{t:#x}" for t in o.targets]
            print(json.dumps(d))
        return
    for o in objs:
        print("  " + describe(o))
        if a.points and o.kind == "batch":
            for oid, pos, s, q in batch_items(o.res.payload):
                print(f"      {oid:#x}  {pos[0]:9.2f} {pos[1]:9.2f} {pos[2]:8.2f}  scale {s:.2f}  "
                      f"q {q[0]:6.3f} {q[1]:6.3f} {q[2]:6.3f} {q[3]:6.3f}")
    if a.deps:
        listed = set(c.block.ids)
        print("  other resources (dependencies):")
        for r in c.resources[1:]:
            if r.id not in listed:
                print(f"    {r.id:#014x}  {forge.fmt_type(r.type):24} {r.name}")


def cmd_find(a):
    """Placements of a prop template (0x51DC6B80, by id or name) or of an instance name substring."""
    mp = Map(a.map)
    want = None
    if re.fullmatch(r"0x[0-9a-fA-F]+", a.what):
        want = int(a.what, 16)
    hits = 0
    cells = sorted(mp.cells)
    for n, (layer, idx) in enumerate(cells):
        fg, e = mp.cells[(layer, idx)]
        if want is not None and want not in forge.header_ids(fg, e):
            continue
        c = mp.cell(idx, layer)
        for o in c.objects():
            if want is not None:
                ok = o.res.id == want or any(r.id == want and r.name in o.props for r in c.resources)
            else:
                ok = a.what.lower() in o.res.name.lower() or any(a.what.lower() in p.lower() for p in o.props)
            if ok:
                hits += 1
                print(f"L{layer} Cell{idx:05d}  " + describe(o))
        if want is None and n % 2000 == 1999:
            print(f"  ... {n + 1}/{len(cells)} cells", file=sys.stderr)
    print(f"{hits} placement(s)", file=sys.stderr)


def write_edit(mp: Map, ce: CellEdit, out_dir: str) -> None:
    c = ce.cell
    res = ce.resources()
    # self-checks: the cell parses again, the datafile packs and parses, and an edit without operations is the original
    new = Cell(c.index, c.archive, c.entry, res, c.layer)
    if any(i in new.by_id and new.by_id[i].type in DATA_CLASSES for i in new.block.ids[:new.block.placed]):
        sys.exit("internal error: placed count")
    df = forge.parse_datafile(forge.build_datafile(res, level=1))
    if [r.payload for r in df.resources] != [r.payload for r in res]:
        sys.exit("internal error: datafile round trip")
    if not ce.log and [r.payload for r in res] != [r.payload for r in c.resources]:
        sys.exit("internal error: an empty edit changed the cell")
    out = os.path.join(out_dir, f"{c.entry.id:016x}_{forge.safe_name(c.name)}")  # the id tells layers apart
    if os.path.exists(out):
        for f in os.listdir(out):
            if f.endswith(".payload") or f == "datafile.json":
                os.remove(os.path.join(out, f))
    forge.write_unpacked(out, c.entry.id, c.name, c.entry.type, res, world={
        "map": mp.name, "layer": c.layer, "cell": c.index, "from": c.archive, "edits": ce.log}, **ce.more)
    for line in ce.log:
        print(f"  {line}")
    print(f"{out}/: {c.name} ({c.archive}), {len(res)} resources, {len(new.block.ids)} listed objects"
          + (", own prefetch blob" if "prefetch" in ce.more else ""))


def cmd_edit(a):
    mp = Map(a.map)
    mp.keep_cells = True
    c = mp.cell(a.cell, a.layer)
    warnings = []

    def warn(s):
        warnings.append(s)
        print(f"warning: {s}", file=sys.stderr)

    ce, targets = edit_cell(mp, c, a, warn)
    edits = [ce] + (edit_proxies(mp, c, targets, a.proxy_radius, warn) if targets else [])
    for e in edits:
        write_edit(mp, e, a.out)
    if warnings:
        print(f"{len(warnings)} warning(s)")


# --- index and batch edits (mods/world_editor) ---------------------------------------------------------------

INDEX_VERSION = 1
IDENTITY = [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]


def index_cell(mp: Map, key: tuple[int, int]) -> list[str]:
    """Index lines of a cell's placed entities and groups (packed instances, proxies and data objects left out):
    obj, layer, cell, id, kind, x, y, z, yaw, scale, bounding-box size x/y/z, name, prop."""
    layer, idx = key
    c = mp.cell(idx, layer)
    rids = set(c.by_id)
    out = []
    for o in c.objects():
        if o.kind not in ("entity", "group") or o.matrix_at is None:
            continue
        m = o.matrix
        if not is_affine(m) or o.kind == "group" and m == IDENTITY:
            continue
        (x, y, z), sc, (yaw, _, _) = decompose(m)
        size = (0.0, 0.0, 0.0)
        p = o.res.payload
        for off, _, cls in object_headers(p, rids - {o.res.id}):
            if cls == AABB_CLASS and off + 36 <= len(p):
                lo, hi = struct.unpack_from("<3f", p, off + 12), struct.unpack_from("<3f", p, off + 24)
                if all(math.isfinite(v) for v in lo + hi):
                    size = tuple(max(0.0, hi[k] - lo[k]) for k in range(3))
                break
        name, prop = (re.sub(r"[\t\r\n]", " ", t) for t in (o.res.name, o.props[0] if o.props else ""))
        out.append("\t".join(["obj", str(layer), str(idx), f"{o.res.id:#x}", o.kind, f"{x:.2f}", f"{y:.2f}",
                              f"{z:.2f}", f"{yaw:.1f}", f"{sum(sc) / 3:.3f}", *(f"{v:.1f}" for v in size), name,
                              prop]))
    return out


_INDEX_MAP: Map | None = None


def _index_init(base: str) -> None:
    global _INDEX_MAP
    _INDEX_MAP = Map(base)  # a worker's own file handles (forked ones would share their offsets)


def _index_part(keys: list[tuple[int, int]]) -> list[str]:
    out = []
    for key in keys:
        try:
            out += index_cell(_INDEX_MAP, key)
        except (ValueError, struct.error) as ex:
            print(f"warning: L{key[0]} Cell{key[1]:05d}: {ex}", file=sys.stderr)
    return out


def cmd_index(a):
    """Every placed entity and group of a map, one tab-separated line each, for the world editor's panel."""
    global _INDEX_MAP
    mp = Map(a.map)
    keys = sorted(mp.cells)
    jobs = max(1, a.jobs or min(16, os.cpu_count() or 1))
    parts = [keys[i::jobs * 4] for i in range(jobs * 4)]
    lines = []
    if jobs > 1:
        import multiprocessing
        ctx = multiprocessing.get_context("fork" if "fork" in multiprocessing.get_all_start_methods() else "spawn")
        with ctx.Pool(jobs, _index_init, (mp.base,)) as pool:
            for part in pool.imap_unordered(_index_part, parts):
                lines += part
    else:
        _INDEX_MAP = mp
        for part in parts:
            lines += _index_part(part)
    lines.sort(key=lambda ln: (int(ln.split("\t")[1]), int(ln.split("\t")[2]), ln))
    head = [f"# world.py index v{INDEX_VERSION}: obj layer cell id kind x y z yaw scale size_x size_y size_z name prop",
            f"map\t{mp.name}\t" + ",".join(os.path.basename(fg.path) for fg in mp.archives)]
    head += [f"grid\t{g.layer}\t{g.min_x:g}\t{g.min_y:g}\t{g.cell:g}\t{g.levels}" for g in mp.layers]
    head += [f"cell\t{layer}\t{idx}" for layer, idx in keys]
    tmp = a.out + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(head + lines) + "\n")
    os.replace(tmp, a.out)
    print(f"{a.out}: {len(lines)} objects in {len(keys)} cells of {mp.name}")


def map_key(name: str) -> str:
    """"DataPC_STP_Alpes" / "Alpes" / "alpes" -> "alpes"."""
    return re.sub(r"^DataPC_STP_", "", name, flags=re.I).lower()


def fold_edits(edits: list[dict], warn) -> tuple[dict, dict]:
    """The world editor's edit list folded, in order, into the final state: added copies {n: {src: (layer, cell,
    id), pos, yaw, scale, name}} and changes of shipped objects {(layer, cell, id): {pos, yaw, scale, removed}}
    (pos None: not moved). An edit of a removed or unknown object is skipped."""
    adds: dict[int, dict] = {}
    objs: dict[tuple[int, int, int], dict] = {}
    for e in sorted(edits, key=lambda e: e.get("n", 0) if isinstance(e.get("n"), int) else 0):
        n = e.get("n", "?")
        try:
            op = e["op"]
            if op == "add":
                s = e["src"]
                adds[int(n)] = {"src": (int(s.get("layer", 0)), int(s["cell"]), int(str(s["id"]), 16)),
                                "pos": [float(v) for v in e["pos"][:3]], "yaw": float(e.get("yaw", 0)),
                                "scale": float(e.get("scale", 1)), "name": str(e.get("name", ""))}
                continue
            if "of" in e:
                st = adds.get(int(e["of"]))
                if st is None:
                    warn(f"edit {n}: {op} of added object {e['of']}, which is gone: skipped")
                    continue
            else:
                o = e["obj"]
                key = (int(o.get("layer", 0)), int(o["cell"]), int(str(o["id"]), 16))
                st = objs.setdefault(key, {"pos": None, "yaw": 0.0, "scale": 1.0, "removed": False})
                if st["removed"]:
                    warn(f"edit {n}: {op} of removed object {o['id']}: skipped")
                    continue
            if op == "move":
                pos = [float(v) for v in e["pos"][:3]]
                if len(pos) != 3:
                    raise ValueError("pos needs x, y, z")
                st["pos"] = pos
            elif op == "turn":
                st["yaw"] += float(e["deg"])
            elif op == "scale":
                st["scale"] *= float(e["factor"])
            elif op == "remove":
                if "of" in e:
                    del adds[int(e["of"])]
                else:
                    st["removed"] = True
            else:
                warn(f"edit {n}: unknown op {op!r}: skipped")
        except (KeyError, TypeError, ValueError, IndexError, AttributeError) as ex:
            warn(f"edit {n}: malformed ({ex!r}): skipped")
    for st in list(adds.values()) + list(objs.values()):
        pos_ok = st["pos"] is None or len(st["pos"]) == 3 and all(math.isfinite(v) and abs(v) < 1e5 for v in st["pos"])
        if not pos_ok or not math.isfinite(st["yaw"]) or not 0.01 <= st["scale"] <= 100:
            warn(f"an object ends up at {st['pos']}, turned {st['yaw']}, scale {st['scale']}: left out")
            st["bad"] = True
    return ({n: st for n, st in adds.items() if not st.get("bad")},
            {k: st for k, st in objs.items() if not st.get("bad")})


def cmd_apply(a):
    """A list of world-editor edits (JSON: a list, or {"edits": [...]}) applied to this map: every cell it changes
    (and the neighbours whose proxies follow) written to -o, as `edit` writes them."""
    mp = Map(a.map)
    mp.keep_cells = True
    with open(a.edits, encoding="utf-8") as f:
        data = json.load(f)
    edits = data.get("edits", []) if isinstance(data, dict) else data
    edits = [e for e in edits if isinstance(e, dict) and map_key(str(e.get("map", ""))) == map_key(mp.name)]
    warnings = []

    def warn(s):
        warnings.append(s)
        print(f"warning: {s}", file=sys.stderr)

    adds, objs = fold_edits(edits, warn)
    cells: dict[tuple[int, int], CellEdit] = {}
    moved: list[tuple[Cell, dict]] = []
    per_cell: dict[tuple[int, int], dict[int, dict]] = collections.defaultdict(dict)
    for (layer, idx, oid), st in objs.items():
        per_cell[(layer, idx)][oid] = st
    for (layer, idx), changes in sorted(per_cell.items()):
        if (layer, idx) not in mp.cells:
            warn(f"L{layer} Cell{idx:05d}: no such cell in {mp.name}: {len(changes)} change(s) skipped")
            continue
        c = mp.cell(idx, layer)
        listed = set(c.block.ids)
        ns = argparse.Namespace(remove=[], move=[], offset=[], yaw=[], scale=[], copy=[], source=None, layer=layer,
                                id=None, copy_no=0)
        for oid, st in changes.items():
            if oid not in listed:
                warn(f"L{layer} Cell{idx:05d}: no object {oid:#x}: its change is skipped")
            elif st["removed"]:
                ns.remove.append(f"{oid:#x}")
            else:
                if st["pos"] is not None:
                    ns.move.append((f"{oid:#x}", ",".join(repr(v) for v in st["pos"])))
                if st["yaw"]:
                    ns.yaw.append((f"{oid:#x}", repr(st["yaw"])))
                if st["scale"] != 1.0:
                    ns.scale.append((f"{oid:#x}", repr(st["scale"])))
        try:
            ce, targets = edit_cell(mp, c, ns, warn)
        except SystemExit as ex:
            warn(f"L{layer} Cell{idx:05d}: {ex}: its changes are skipped")
            continue
        cells[(layer, idx)] = ce
        if targets:
            moved.append((c, targets))
    sources: dict[tuple[int, int], Cell] = {}
    copies: collections.Counter = collections.Counter()
    for n, st in sorted(adds.items()):
        x, y = st["pos"][0], st["pos"][1]
        # the finest cell of the main table there (empty slots have no datafile to add to)
        where = next(((0, i) for lvl in range(mp.grid.levels) if (i := mp.grid.at(x, y, lvl)) is not None
                      and (0, i) in mp.cells), None)
        sl, sc, sid = st["src"]
        what = f"add {n} ({st['name'] or hex(sid)})"
        if where is None:
            warn(f"{what}: no cell of {mp.name} covers ({x:.1f}, {y:.1f}): skipped")
            continue
        if (sl, sc) not in mp.cells:
            warn(f"{what}: its source cell L{sl} {sc} is not in {mp.name}: skipped")
            continue
        if copies[where] > 0xFF:
            warn(f"{what}: more than 256 copies in cell {where[1]}: skipped")
            continue
        try:
            if (sl, sc) not in sources:
                sources[(sl, sc)] = mp.cell(sc, sl)
            src = sources[(sl, sc)]
            pl = src.placed_object(sid) if sid in src.block.ids else None
            if pl is None:
                warn(f"{what}: no object {sid:#x} in its source cell {sc}: skipped")
                continue
            if where not in cells:
                cells[where] = CellEdit(mp.cell(where[1], 0))
            add_copy(mp, cells[where], src, pl, st["pos"], st["yaw"], st["scale"],
                     NEW_ID_BASE + (where[1] << 20) + (copies[where] << 12), warn)
            copies[where] += 1
        except SystemExit as ex:
            warn(f"{what}: {ex}: skipped")
    for c, targets in moved:
        edit_proxies(mp, c, targets, a.proxy_radius, warn, cells)
    os.makedirs(a.out, exist_ok=True)
    written = 0
    for key in sorted(cells):
        ce = cells[key]
        finish_prefetch(ce)
        if ce.log:
            write_edit(mp, ce, a.out)
            written += 1
    print(f"{mp.name}: {len(edits)} edit(s): {len(adds)} added object(s), {len(objs)} changed; {written} cell(s) "
          f"written, {len(warnings)} warning(s)")


def cmd_check(a):
    """Parse every cell of a map and check what WORLD.md states (C) on it."""
    mp = Map(a.map)
    g = mp.grid
    stats = {"cells": 0, "objects": 0, "placed_first": 0, "inside": 0, "outside": 0, "proxy_ok": 0, "proxy_odd": 0,
             "group_children": 0, "batch_points": 0, "roundtrip": 0, "placed_sorted": 0, "resources_in_list_order": 0,
             "identity_groups": 0}
    by_level: dict[int, list[int]] = {}
    own_seen: dict[int, int] = {}
    problems, outside, proxies = [], [], []
    listed: dict[int, int] = {}
    in_prefix, in_rest = collections.Counter(), collections.Counter()
    cells = sorted(mp.cells)[:a.limit] if a.limit else sorted(mp.cells)
    for n, (layer, idx) in enumerate(cells):
        try:
            c = mp.cell(idx, layer)
        except (ValueError, struct.error) as ex:
            problems.append(f"L{layer} Cell{idx:05d}: {ex}")
            continue
        stats["cells"] += 1
        if c.block.index != idx:
            problems.append(f"L{layer} Cell{idx:05d}: block index {c.block.index}")
        if c.block.build() != c.resources[0].payload:
            problems.append(f"Cell{idx:05d}: DataBlock does not rebuild byte for byte")
        else:
            stats["roundtrip"] += 1
        for k, i in enumerate(c.block.ids):
            (in_prefix if k < c.block.placed else in_rest)[c.by_id[i].type if i in c.by_id else "missing"] += 1
        kinds = [i not in c.by_id or c.by_id[i].type not in DATA_CLASSES for i in c.block.ids]
        if sum(kinds) != c.block.placed:
            problems.append(f"Cell{idx:05d}: placed {c.block.placed}, not data classes {sum(kinds)}")
        if kinds == sorted(kinds, reverse=True):
            stats["placed_first"] += 1
        head = c.block.ids[:c.block.placed]
        stats["placed_sorted"] += head == sorted(head)
        stats["resources_in_list_order"] += [r.id for r in c.resources[1:1 + len(c.block.ids)]] == c.block.ids
        lvl = g.locate(idx)[0]
        rids = set(c.by_id)
        for o in c.objects():
            stats["objects"] += 1
            listed[o.res.id] = (layer, idx)
            if o.kind == "proxy":
                if o.matrix_at is None:
                    stats["proxy_odd"] += 1
                else:
                    proxies += [((layer, idx), t) for t in o.targets]
                continue
            if o.kind == "batch":
                stats["batch_points"] += o.items
                continue
            if o.matrix_at is None:
                continue
            m = o.matrix
            if not is_affine(m):
                problems.append(f"Cell{idx:05d}: {o.res.name}: matrix not affine")
                continue
            if o.kind == "group" and m == [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]:
                stats["identity_groups"] += 1  # packed instances: their content carries the positions
                continue
            x0, y0, size = g.rect(idx)
            off = max(x0 - m[12], m[12] - x0 - size, y0 - m[13], m[13] - y0 - size, 0)
            inside = off <= a.margin
            stats["inside" if inside else "outside"] += 1
            if not inside:
                outside.append((off, f"L{layer} Cell{idx:05d} {o.res.name} {fmt_pos(m)}: {off:.1f} m out"))
            by_level.setdefault(lvl, [0, 0])[0 if inside else 1] += 1
            stats["group_children"] += len(o.children)
            if a.ids:
                for v in own_ids(o.res, rids):
                    own_seen[v] = own_seen.get(v, 0) + 1
        if n % 2000 == 1999:
            print(f"  ... {n + 1}/{len(cells)} cells", file=sys.stderr)
    print(f"{mp.name}: {stats['cells']} cells, {stats['objects']} listed objects")
    print(f"  DataBlocks rebuilt byte for byte: {stats['roundtrip']}/{stats['cells']}; placed objects listed first: "
          f"{stats['placed_first']}/{stats['cells']}, sorted by id: {stats['placed_sorted']}; resources in list order: "
          f"{stats['resources_in_list_order']}")
    print(f"  entity/group origins inside their cell's square (+-{a.margin:g} m): {stats['inside']}, outside: "
          f"{stats['outside']}")
    for lvl in sorted(by_level):
        i, o = by_level[lvl]
        print(f"    level {lvl}: {i} inside, {o} outside")
    print(f"  identity-matrix groups (packed instances) left out: {stats['identity_groups']}")
    for _, line in sorted(outside, reverse=True)[:8]:
        print(f"    furthest out: {line}")
    same = sum(1 for key, t in proxies if listed.get(t) == key)
    other = sum(1 for key, t in proxies if t in listed and listed[t] != key)
    print(f"  group child entities: {stats['group_children']}; proxy targets: {len(proxies)} (listed "
          f"in the same cell {same}, in another cell {other}, nowhere {len(proxies) - same - other}), "
          f"{stats['proxy_odd']} without; batch points: {stats['batch_points']}")
    rel = collections.Counter()
    for key, t in proxies:
        if t in listed and listed[t] != key:
            (la, ca, ra), (lb, cb, rb) = g.locate(key[1]), g.locate(listed[t][1])
            if key[0] != listed[t][0]:
                rel["other layer"] += 1
            elif la == lb:
                rel[f"same level, {max(abs(ca - cb), abs(ra - rb))} cell(s) away"] += 1
            else:
                rel[f"target {lb - la:+d} level(s)"] += 1
    if rel:
        print("  proxy -> target cell: " + ", ".join(f"{k}: {v}" for k, v in rel.most_common()))
    for label, cnt in (("in the placed prefix", in_prefix), ("after it", in_rest)):
        print(f"  classes {label}: " + ", ".join(f"{KINDS.get(t, t if isinstance(t, str) else hex(t))} {k}"
                                                 for t, k in cnt.most_common()))
    if a.ids:
        dup = [v for v, k in own_seen.items() if k > 1]
        top = max(own_seen) if own_seen else 0
        print(f"  own ids: {len(own_seen)}, in more than one instance: {len(dup)}"
              + (f" (e.g. {', '.join(hex(v) for v in dup[:4])})" if dup else "")
              + f"; largest {top:#x} (copies start at {NEW_ID_BASE:#x})")
    top_entry = max(mp.entries)
    print(f"  largest entry id {top_entry:#x}; cell tables (layers): {len(mp.layers)}; unreferenced cell datafiles: "
          f"{len(mp.unreferenced)}")
    for p in problems[:30]:
        print(f"  problem: {p}")
    if len(problems) > 30:
        print(f"  ... {len(problems) - 30} more problems")
    return 1 if problems else 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)

    def m(p):
        p.add_argument("map", help="the map's base archive, e.g. DataPC_STP_WhiteRoom.forge (patches are added)")

    p = sp.add_parser("grid", help="the world's cell tables (layers): levels, cell sizes, extent")
    m(p)
    p.set_defaults(fn=cmd_grid)
    p = sp.add_parser("cells", help="list cell datafiles (index, level, square, archive)")
    m(p)
    p.add_argument("--level", type=int)
    p.add_argument("--layer", type=int, help="only this cell table (layer)")
    p.add_argument("--at", help="X,Y: only the cells (of every level) covering this point")
    p.add_argument("--count", action="store_true", help="also count each cell's objects by kind (reads the cells)")
    p.add_argument("--max", type=int, default=200)
    p.set_defaults(fn=cmd_cells)
    p = sp.add_parser("show", help="a cell's objects: id, kind, name, position, rotation, scale, prop")
    m(p)
    p.add_argument("cell", type=int, help="cell index (the NNNNN of CellNNNNN_DataBlock)")
    p.add_argument("--layer", type=int, help="the cell table (layer), when two have this index")
    p.add_argument("--json", action="store_true", help="one JSON object per listed object (with the full matrix)")
    p.add_argument("--points", action="store_true", help="also list the points of each batch")
    p.add_argument("--deps", action="store_true", help="also list the cell's other resources (props, shapes, ...)")
    p.add_argument("--dir", help="read the cell from this unpacked folder (e.g. edit's output) instead of the map")
    p.set_defaults(fn=cmd_show)
    p = sp.add_parser("find", help="every placement of a prop (0x51DC6B80 id: header scan) or a name substring "
                      "(instance or prop name: reads every cell)")
    m(p)
    p.add_argument("what")
    p.set_defaults(fn=cmd_find)
    p = sp.add_parser("edit", help="write a modified cell (unpacked datafile folder for forge.py build)")
    m(p)
    p.add_argument("cell", type=int)
    p.add_argument("--layer", type=int, help="the cell table (layer), when two have this index")
    p.add_argument("-o", "--out", required=True, help="folder for the result, e.g. mods/<mod>/client/forge/<map>")
    p.add_argument("--remove", action="append", metavar="OBJ", help="drop an object (id or exact name) from the cell")
    p.add_argument("--move", action="append", nargs=2, metavar=("OBJ", "X,Y,Z"), help="put an object's origin here")
    p.add_argument("--offset", action="append", nargs=2, metavar=("OBJ", "DX,DY,DZ"), help="move an object by this")
    p.add_argument("--yaw", action="append", nargs=2, metavar=("OBJ", "DEG"), help="turn an object about Z (its "
                   "origin) by DEG, counter-clockwise seen from above")
    p.add_argument("--scale", action="append", nargs=2, metavar=("OBJ", "S"), help="scale an object about its origin")
    p.add_argument("--copy", action="append", nargs=2, metavar=("OBJ", "X,Y,Z[,YAW]"),
                   help="add a copy of an entity at this position (optionally turned by YAW degrees)")
    p.add_argument("--source", type=int, help="--copy takes OBJ from this cell (its dependencies come along)")
    p.add_argument("--id", type=lambda s: int(s, 0), help="the first copy's id (default: a fresh block, see WORLD.md)")
    p.add_argument("--copy-no", type=int, default=0, help="first copy number within the cell (for separate edits)")
    p.add_argument("--proxy-radius", type=int, default=1, help="look for proxies of moved/removed objects this many "
                   "cells around (default 1; see WORLD.md)")
    p.set_defaults(fn=cmd_edit)
    p = sp.add_parser("index", help="every placed entity and group, one tab-separated line each (world editor)")
    m(p)
    p.add_argument("-o", "--out", required=True, help="the index file to write")
    p.add_argument("--jobs", type=int, help="worker processes (default: the CPUs, at most 16)")
    p.set_defaults(fn=cmd_index)
    p = sp.add_parser("apply", help="write the cells a list of world-editor edits (JSON) changes")
    m(p)
    p.add_argument("edits", help='JSON: [edit, ...] or {"edits": [...]}; edits of other maps are left out')
    p.add_argument("-o", "--out", required=True, help="folder for the edited cells (its map's forge folder)")
    p.add_argument("--proxy-radius", type=int, default=1, help="as for edit")
    p.set_defaults(fn=cmd_apply)
    p = sp.add_parser("check", help="parse every cell, check the layout claims, rebuild DataBlocks")
    m(p)
    p.add_argument("--limit", type=int, help="only the first N cells")
    p.add_argument("--ids", action="store_true", help="also collect own ids (duplicates across instances)")
    p.add_argument("--margin", type=float, default=8.0, help="metres an origin may lie outside its cell (default 8)")
    p.set_defaults(fn=cmd_check)
    a = ap.parse_args(argv)
    return a.fn(a) or 0


if __name__ == "__main__":
    sys.exit(main())
