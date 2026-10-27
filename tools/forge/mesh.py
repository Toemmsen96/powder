#!/usr/bin/env python3
"""Steep's meshes (resource type 0x415D9568): layout, OBJ/glTF export, vertex edits and OBJ/glTF import.

A mesh resource is self-contained: bones (name hash + 4x4 matrix), bounds, the vertex and index buffers (twice:
a per-submesh section and a reordered copy), the submesh table (material, bone map) and the position/UV scales.
`edit` and `import` change vertices and write an unpacked datafile folder (ids kept) that `forge.py build` turns
into an archive, like `forge.py extract`/`swap` output. glTF (.glb) carries the armature and skin weights to and
from Blender (BLENDER.md). The format, and what is confirmed vs guessed, is in MESHES.md next to this file.

Sources: an unpacked folder (datafile.json), a .datafile, a bare .payload, or `ARCHIVE.forge ENTRY` (the entry
from the newest archive of the stack, as the game loads it). Archives are only read. Python 3.10+, stdlib only.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import struct
import sys
import zlib
from dataclasses import dataclass, field

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forge  # noqa: E402  (next to this file)
import gltf  # noqa: E402

MESH_TYPE = 0x415D9568
CLS_BONE = 0x9EF0E7A1
CLS_COMPILED = 0xFC9E1595
CLS_SECTION1 = 0xC351EE43    # per-submesh buffers (indices relative to each submesh's first vertex)
CLS_SECTION2 = 0x0645ABB5    # draw ranges + a reordered copy of the buffers (absolute indices), may be empty
CLS_PRIM = 0xA57387EF        # draw range: first vertex, vertex count, first index, triangle count
CLS_SUBMESH = 0x0B1D34C1     # submesh: material id + bone map (128 u16, unused = 0xF7F7)
BONE_SIZE = 81               # u64 id, u32 class, u32 name hash, 16 f32, u8
SUBMESH_SIZE = 12 + 18 + 256
REC_SIZE = 36                # section 1 per-submesh record: centre, half size, stride, (indices << 8 | 1), first
Q = 32767                    # i16 quantisation

# Vertex formats: format byte -> (stride, {attribute: offset}). Positions are i16x4; normal/tangent/binormal u8x4
# ((b - 127.5) / 127.5); uv i16x2 times the mesh's UV scale / 32767; bones/weights u8x4 (weights sum to 255).
# Offsets not listed are kept as they are (e.g. format 2's +28 is always zero, +40 always ff 80 ff ff).
FORMATS = {
    0: (32, {"normal": 8, "tangent": 12, "binormal": 16, "uv0": 20, "bones": 24, "weights": 28}),
    1: (40, {"normal": 8, "tangent": 12, "binormal": 16, "uv0": 20, "uv1": 24}),
    2: (44, {"normal": 8, "tangent": 12, "binormal": 16, "uv0": 20, "uv1": 24, "bones": 32, "weights": 36}),
    3: (52, {"normal": 8, "tangent": 12, "binormal": 16, "uv0": 20, "uv1": 24}),
    6: (24, {"normal": 8, "tangent": 12, "color": 16, "uv0": 20}),
    8: (20, {"normal": 8, "tangent": 12, "uv0": 16}),
}
SKINNED = {0, 1, 2, 3}       # position = xyz * S / 32767, w = an unrelated per-vertex value
# static formats (6, 8):       position = xyz * w / 32767, w = +-S (the sign is the tangent frame's handedness)

# Bone name hashes are CRC-32 of the names (C). These names were found in the skeleton add-ons' strings or guessed
# (Mixamo-style); the other bones go by their hash, written 0x<8 hex digits>.
_SIDED = ["{}Arm", "{}ForeArm", "{}Hand", "{}Shoulder", "{}UpLeg", "{}Foot", "{}ToeBase", "L_{}Arm_NoRoll",
          "L_{}ForeArm", "L_{}Leg", "L_{}Scapula", "L_{}ShirtBend", "L_{}UpLeg_NoRoll", "L_{}Wrist", "O_{}Arm",
          "O_{}ForeArm", "O_{}Knee", "O_{}UpLegRoll", "P_{}Clavicle", "P_{}Trapezius", "RFX_{}Acromion", "RFX_{}Coat",
          "RFX_{}GMaximus", "RFX_{}KneeBack", "RFX_{}Pectoral"]
_SIDED += [f"{{}}Hand{f}{i}" for f in ("Thumb", "Index", "Middle", "Ring", "Pinky") for i in (1, 2, 3)]
BONE_NAMES = ["Reference", "Hips", "Spine", "Spine1", "Spine2", "Neck", "Neck1", "Head", "Fx_Head", "Fx_L_Ear",
              "Fx_L_Jaw", "Fx_R_Ear", "Fx_R_Jaw", "RFX_RightUpLeg_Upnode"] + [f.format(s) for s in ("Left", "Right")
                                                                              for f in _SIDED]
BONE_BY_HASH = {zlib.crc32(n.encode()): n for n in BONE_NAMES}
# The skeleton resource is not decoded: this hierarchy (Mixamo's) is a guess, only used to pose the armature in
# Blender. A helper bone (L_LeftArm_NoRoll, O_LeftKnee, ...) hangs off the bone its name points at, any other bone
# off the nearest bone that has an entry.
PARENTS = {"Hips": "Reference", "Spine": "Hips", "Spine1": "Spine", "Spine2": "Spine1", "Neck": "Spine2",
           "Neck1": "Neck", "Head": "Neck1"}
for _s in ("Left", "Right"):
    PARENTS.update({f"{_s}Shoulder": "Spine2", f"{_s}Arm": f"{_s}Shoulder", f"{_s}ForeArm": f"{_s}Arm",
                    f"{_s}Hand": f"{_s}ForeArm", f"{_s}UpLeg": "Hips", f"{_s}Leg": f"{_s}UpLeg",
                    f"{_s}Knee": f"{_s}Leg", f"{_s}Foot": f"{_s}Leg", f"{_s}Wrist": f"{_s}ForeArm",
                    f"{_s}ToeBase": f"{_s}Foot"})
    for _f in ("Thumb", "Index", "Middle", "Ring", "Pinky"):
        PARENTS.update({f"{_s}Hand{_f}1": f"{_s}Hand", f"{_s}Hand{_f}2": f"{_s}Hand{_f}1",
                        f"{_s}Hand{_f}3": f"{_s}Hand{_f}2"})


def bone_name(h: int) -> str:
    return BONE_BY_HASH.get(h, f"0x{h:08x}")


def bone_hash(name: str) -> int:
    """A bone's hash from its name as Blender writes it back (a `.001` suffix from a duplicate is dropped)."""
    name = re.sub(r"\.\d{3}$", "", name)
    if re.fullmatch(r"0x[0-9a-fA-F]{8}", name):
        return int(name, 16)
    return zlib.crc32(name.encode())


class MeshError(ValueError):
    pass


class Reader:
    def __init__(self, data: bytes, pos: int = 0):
        self.d, self.o = data, pos

    def take(self, fmt: str):
        v = struct.unpack_from(fmt, self.d, self.o)
        self.o += struct.calcsize(fmt)
        return v if len(v) > 1 else v[0]

    def obj(self, cls: int) -> int:
        oid, c = struct.unpack_from("<QI", self.d, self.o)
        if c != cls:
            raise MeshError(f"expected class {cls:#010x} at {self.o + 8:#x}, found {c:#010x}")
        self.o += 12
        return oid


@dataclass
class Buf:
    off: int     # payload offset of the data (after its u32 size)
    size: int


@dataclass
class Submesh:
    index: int
    first_vertex: int
    vertex_count: int
    first_index: int
    triangles: int
    material: int = 0
    bone_map: list = field(default_factory=list)
    table_off: int | None = None  # payload offset of its 18-byte submesh-table body


@dataclass
class Mesh:
    data: bytes
    id: int
    flag: int                     # header byte 0x0D: 0 on static meshes, 1 or 2 on skinned ones (G)
    bones: list                   # (name hash, 16 floats)
    bbox_off: int                 # 8 f32: min xyz 0, max xyz 0
    fmt: int
    stride: int
    s1: dict | None               # section 1: center_off, n, a2_off, vb, ib, recs_off
    vb2: Buf
    ib2: Buf
    idx32: bool                   # section 2 indices are u32
    prims: list                   # section 2's two draw-range lists, (first v, 0, v count, first i, tris, 1)
    prims_off: list               # payload offsets of those 24-byte records
    submeshes: list               # Submesh, in section-2 draw order
    sub_off: int                  # offset of the submesh table (u32 count)
    scale_off: int                # f32 S (positions), f32 X (UVs)
    S: float
    X: float

    @property
    def main(self) -> tuple[Buf, Buf, bool]:
        """The buffers to read: section 1 (base-vertex indices) when it has data, else section 2 (absolute)."""
        if self.s1 and self.s1["vb"].size:
            return self.s1["vb"], self.s1["ib"], True
        return self.vb2, self.ib2, False

    @property
    def vertex_count(self) -> int:
        return self.main[0].size // self.stride

    def attrs(self) -> dict:
        return FORMATS.get(self.fmt, (self.stride, {}))[1]


def parse_mesh(p: bytes) -> Mesh:
    r = Reader(p)
    mid, typ = r.take("<QI")
    if typ != MESH_TYPE:
        raise MeshError(f"type {typ:#010x} is not a mesh")
    flag = p[0x0D]
    r.o = 0x17
    nb = r.take("<I")
    bones = []
    for _ in range(nb):
        r.obj(CLS_BONE)
        h = r.take("<I")
        m = struct.unpack_from("<16f", p, r.o)
        r.o += 64 + 1
        bones.append((h, m))
    bbox_off = r.o
    r.o += 32 + 1
    r.obj(CLS_COMPILED)
    r.o += 5                                   # u32 0, u8 (0 or 3; 3 = no section 1, G)
    oid, cls = struct.unpack_from("<QI", p, r.o)
    s1 = None
    if cls == CLS_SECTION1:
        r.o += 12
        _, fmt1, stride1, _ = r.take("<IBII")
        center_off = r.o
        r.o += 24
        n = r.take("<I")
        n1 = r.take("<I")
        r.o += 4 * n1
        n2 = r.take("<I")
        a2_off = r.o
        r.o += 4 * n2 + 1
        vbs = r.take("<I")
        vb1 = Buf(r.o, vbs)
        r.o += vbs
        ibs = r.take("<I")
        ib1 = Buf(r.o, ibs)
        r.o += ibs
        rl = r.take("<I")
        if rl != REC_SIZE * n or n1 != n or n2 != n:
            raise MeshError(f"section 1: {n} submeshes but tables of {n1}, {n2}, {rl} bytes")
        s1 = {"fmt": fmt1, "stride": stride1, "center_off": center_off, "n": n, "a2_off": a2_off,
              "vb": vb1, "ib": ib1, "recs_off": r.o}
        r.o += rl
    r.obj(CLS_SECTION2)
    idx32, fmt, stride = r.take("<BBB")
    prims, prims_off = [], []
    for _ in range(2):
        k = r.take("<I")
        lst, offs = [], []
        for _ in range(k):
            r.obj(CLS_PRIM)
            offs.append(r.o)
            lst.append(r.take("<6I"))
        prims.append(lst)
        prims_off.append(offs)
    vbs = r.take("<I")
    vb2 = Buf(r.o, vbs)
    r.o += vbs
    ibs = r.take("<I")
    ib2 = Buf(r.o, ibs)
    r.o += ibs
    if s1 and (s1["fmt"] != fmt or s1["stride"] != stride):
        raise MeshError("the two sections disagree on the vertex format")
    if fmt in FORMATS and FORMATS[fmt][0] != stride:
        raise MeshError(f"format {fmt} with stride {stride}")
    sub_off = r.o
    k = r.take("<I")
    subs = []
    for _ in range(k):
        r.obj(CLS_SUBMESH)
        body = p[r.o:r.o + 18]
        prim = struct.unpack_from("<H", body, 5)[0]
        mat = struct.unpack_from("<Q", body, 10)[0]
        bm = [b for b in struct.unpack_from("<128H", p, r.o + 18) if b != 0xF7F7]
        fv, _, vc, fi, tc, _ = prims[0][prim] if prim < len(prims[0]) else (0, 0, 0, 0, 0, 0)
        subs.append(Submesh(prim, fv, vc, fi, tc, mat, bm, r.o))
        r.o += 18 + 256
    known = {s.index for s in subs}
    for i, (fv, _, vc, fi, tc, _) in enumerate(prims[0]):  # draw ranges without a table entry (not seen)
        if i not in known:
            subs.append(Submesh(i, fv, vc, fi, tc))
    subs.sort(key=lambda s: s.index)
    a, b = r.take("<II")
    if (a, b) != (26, 7):
        raise MeshError(f"unexpected fields {a}, {b} before the scales at {r.o - 8:#x}")
    scale_off = r.o
    S, X = r.take("<2f")
    return Mesh(p, mid, flag, bones, bbox_off, fmt, stride, s1, vb2, ib2, bool(idx32), prims, prims_off,
                subs, sub_off, scale_off, S, X)


# --- vertices ---------------------------------------------------------------------------------------------------

def rows(m: Mesh, b: Buf) -> list[bytes]:
    return [m.data[b.off + i:b.off + i + m.stride] for i in range(0, b.size, m.stride)]


def dec_pos(m: Mesh, row: bytes) -> tuple[float, float, float]:
    x, y, z, w = struct.unpack_from("<4h", row)
    s = m.S if m.fmt in SKINNED else w
    return (x * s / Q, y * s / Q, z * s / Q)


def enc_pos(m: Mesh, row: bytes, pos, scale: float | None = None) -> bytes:
    """`row` with its position set to `pos` (positions quantised with S, or the row's own w for static meshes)."""
    x, y, z, w = struct.unpack_from("<4h", row)
    s = scale if scale is not None else m.S
    if m.fmt not in SKINNED:
        w = int(math.copysign(round(s), w)) if scale is not None else w
        s = w
    q = [round(c * Q / s) for c in pos]
    if any(abs(c) > Q for c in q):
        raise MeshError(f"position {pos} is outside the mesh's range of +-{abs(s)} m")
    return struct.pack("<4h", *q, w) + row[8:]


def dec_vec(row: bytes, off: int) -> tuple[float, float, float]:
    return tuple((row[off + i] - 127.5) / 127.5 for i in range(3))


def enc_vec(v) -> bytes:
    return bytes(max(0, min(255, round(c * 127.5 + 127.5))) for c in v)


def dec_uv(m: Mesh, row: bytes, off: int) -> tuple[float, float]:
    u, v = struct.unpack_from("<2h", row, off)
    return (u * m.X / Q, v * m.X / Q)


def norm(v):
    n = math.sqrt(sum(c * c for c in v))
    return tuple(c / n for c in v) if n > 1e-12 else (0.0, 0.0, 1.0)


def dot(a, b):
    return sum(x * y for x, y in zip(a, b))


def triangles(m: Mesh, sm: Submesh) -> list[tuple[int, int, int]]:
    """A submesh's triangles as vertex numbers of the main buffer."""
    vb, ib, relative = m.main
    w = 4 if (m.idx32 and not relative) else 2
    base = sm.first_vertex if relative else 0
    idx = struct.unpack_from(f"<{3 * sm.triangles}{'I' if w == 4 else 'H'}", m.data, ib.off + w * sm.first_index)
    return [(base + idx[i], base + idx[i + 1], base + idx[i + 2]) for i in range(0, len(idx), 3)]


def describe_format(m: Mesh) -> str:
    attrs = m.attrs()
    parts = [f"pos i16x4 @0 ({'x S/32767' if m.fmt in SKINNED else 'x w/32767, w = +-S'})"]
    kinds = {"normal": "u8x4", "tangent": "u8x4", "binormal": "u8x4", "color": "u8x4", "uv0": "i16x2",
             "uv1": "i16x2", "bones": "u8x4", "weights": "u8x4"}
    known = {0, 4}
    for a, off in sorted(attrs.items(), key=lambda kv: kv[1]):
        parts.append(f"{a} {kinds[a]} @{off}")
        known.update(range(off, off + 4))
    rest = [o for o in range(0, m.stride, 4) if o not in known]
    if rest:
        parts.append("unknown " + ", ".join(f"@{o}" for o in rest))
    return ", ".join(parts)


def bounds(pts):
    lo = [min(p[i] for p in pts) for i in range(3)]
    hi = [max(p[i] for p in pts) for i in range(3)]
    return lo, hi


def center_half(lo, hi) -> bytes:
    """Section 1's box form: centre and half size (6 f32)."""
    return struct.pack("<6f", *[(a + b) / 2 for a, b in zip(lo, hi)], *[(b - a) / 2 for a, b in zip(lo, hi)])


# --- sources ------------------------------------------------------------------------------------------------------

@dataclass
class Source:
    label: str
    entry: int
    name: str
    type: int
    resources: list               # forge.Resource
    manifest: dict | None = None  # datafile.json of an unpacked folder


def game_entry(base: str, spec: str) -> tuple[forge.Forge, forge.Entry]:
    """An entry from the newest archive of `base`'s stack, as the game loads it, leaving out the archives a
    powder-wrap session put there (listed in .powder-assets): editing a mod's own output again would stack edits."""
    try:
        with open(os.path.join(os.path.dirname(os.path.abspath(base)), ".powder-assets")) as f:
            session = {line.strip() for line in f if line.strip()}
    except OSError:
        session = set()
    archives = []
    for p in reversed([p for p in forge.patch_stack(base) if os.path.exists(p)]):
        if os.path.basename(p) in session:
            print(f"  ({os.path.basename(p)} is a powder-wrap session's, left out)", file=sys.stderr)
        else:
            archives.append(forge.Forge(p))
    for exact in (True, False):  # an id or exact name anywhere in the stack wins over a substring in a patch
        for fg in archives:
            found = fg.find(spec, exact)
            if len(found) > 1:
                sys.exit(f"{spec}: {len(found)} entries in {os.path.basename(fg.path)} match; give the id or the "
                         "exact name")
            if found:
                return fg, found[0]
    sys.exit(f"{spec}: in no archive of {base}'s stack")


def load_source(src: str, entry: str | None) -> Source:
    if src.endswith(".forge"):
        if not entry:
            sys.exit(f"{src}: name the entry (id or exact name)")
        fg, e = game_entry(src, entry)
        df = forge.parse_datafile(fg.read(e))
        return Source(f"{os.path.basename(fg.path)}:{e.name}", e.id, e.name, e.type, df.resources)
    if os.path.isdir(src):
        m, res = forge.load_unpacked(src)
        return Source(src, int(m["entry"], 16), m["name"], int(m["type"], 16), res, m)
    with open(src, "rb") as f:
        data = f.read()
    if forge.is_datafile(data):
        res = forge.parse_datafile(data).resources
        r0 = res[0]
        return Source(src, r0.id, r0.name, r0.type, res)
    mid, typ = struct.unpack_from("<QI", data)
    name = re.sub(r"^\d{3}_[0-9a-f]{16}_|\.payload$", "", os.path.basename(src))  # as write_unpacked names them
    return Source(src, mid, name, typ, [forge.Resource(mid, typ, name, data)])


def mesh_resources(s: Source, only: list[str] | None = None) -> list[tuple[int, forge.Resource]]:
    out = []
    for k, r in enumerate(s.resources):
        if r.type != MESH_TYPE:
            continue
        if only and not any(f.lower() in r.name.lower() or (re.fullmatch(r"0x[0-9a-fA-F]+", f) and int(f, 16) == r.id)
                            for f in only):
            continue
        out.append((k, r))
    if not out:
        sys.exit(f"{s.label}: no mesh resources" + (" match" if only else ""))
    return out


def names_by_id(s: Source) -> dict[int, str]:
    return {r.id: r.name for r in s.resources}


# --- info -------------------------------------------------------------------------------------------------------

def info(m: Mesh, name: str, names: dict[int, str], out=print):
    vb, ib, relative = m.main
    out(f"{name}  id={m.id:#x}  {len(m.data)} bytes  {'skinned' if m.fmt in SKINNED else 'static'}, "
        f"byte 0x0D = {m.flag}")
    bb = struct.unpack_from("<8f", m.data, m.bbox_off)
    out(f"  bounds    min ({bb[0]:.3f}, {bb[1]:.3f}, {bb[2]:.3f})  max ({bb[4]:.3f}, {bb[5]:.3f}, {bb[6]:.3f}) m"
        f"  (size {bb[4] - bb[0]:.3f} x {bb[5] - bb[1]:.3f} x {bb[6] - bb[2]:.3f})")
    out(f"  scales    positions S = {m.S:g} m, UVs X = {m.X:.6g}")
    out(f"  format    {m.fmt}, stride {m.stride}: {describe_format(m)}")
    if m.s1:
        out(f"  section 1 {m.s1['vb'].size // m.stride} vertices, {m.s1['ib'].size // 2} indices (u16, relative to "
            f"each submesh's first vertex)")
    v2 = m.vb2.size // m.stride
    w2 = 4 if m.idx32 else 2
    out(f"  section 2 {v2} vertices, {m.ib2.size // w2} indices ({'u32' if m.idx32 else 'u16'}, absolute)"
        + (", the same vertices as section 1 (reordered in shipped data)" if m.s1 and v2 else
           ", empty" if not v2 else ""))
    if m.prims[1] != m.prims[0]:
        out(f"            second draw list: {[(p[0], p[2], p[3], p[4]) for p in m.prims[1]]} (first v, verts, "
            f"first i, tris)")
    if m.bones:
        out(f"  bones     {len(m.bones)}: name hashes {', '.join(f'{h:08x}' for h, _ in m.bones[:6])}"
            + (", ..." if len(m.bones) > 6 else ""))
    out(f"  submeshes {len(m.submeshes)}:")
    for s in m.submeshes:
        mat = names.get(s.material, "")
        out(f"    {s.index}: vertices {s.first_vertex}..{s.first_vertex + s.vertex_count - 1} ({s.vertex_count}), "
            f"{s.triangles} triangles, material {s.material:#x} {mat}"
            + (f", bone map {len(s.bone_map)}" if s.bone_map else ""))
    r = rows(m, vb)
    pts = [dec_pos(m, x) for x in r]
    if pts:
        lo, hi = bounds(pts)
        out(f"  decoded   min ({lo[0]:.3f}, {lo[1]:.3f}, {lo[2]:.3f})  max ({hi[0]:.3f}, {hi[1]:.3f}, {hi[2]:.3f})")
    a = m.attrs()
    if "weights" in a and r:
        infl = [sum(1 for w in x[a["weights"]:a["weights"] + 4] if w) for x in r]
        out(f"  skinning  {'/'.join(str(infl.count(k)) for k in range(1, 5))} vertices with 1/2/3/4 bones "
            f"(u8 bone slots into the submesh's bone map, u8 weights summing to 255)")


def cmd_info(a):
    s = load_source(a.src, a.entry)
    names = names_by_id(s)
    print(f"{s.label}: entry {s.entry:#x} {s.name}, {len(s.resources)} resource(s)")
    for _, r in mesh_resources(s, a.mesh):
        info(parse_mesh(r.payload), r.name, names)


# --- export -----------------------------------------------------------------------------------------------------

def export_obj(m: Mesh, name: str, sm: Submesh, path: str, names: dict[int, str]) -> tuple[int, int]:
    vb, _, _ = m.main
    a = m.attrs()
    r = rows(m, vb)
    tris = triangles(m, sm)
    first, n = sm.first_vertex, sm.vertex_count
    lines = [f"# Steep mesh {name} ({m.id:#x}), submesh {sm.index}, material {sm.material:#x} "
             f"{names.get(sm.material, '')}".rstrip(),
             f"# {n} vertices (main-buffer vertices {first}..{first + n - 1}), {len(tris)} triangles; "
             "Z up, metres; vt = (u, 1 - v); vn = the stored normals as decoded (not renormalised)",
             "# written by tools/forge/mesh.py export; `mesh.py import` maps v lines back to these vertices in order",
             f"o {name}_sm{sm.index}"]
    for row in r[first:first + n]:
        x, y, z = dec_pos(m, row)
        lines.append(f"v {x:.6f} {y:.6f} {z:.6f}")
    has_uv, has_n = "uv0" in a, "normal" in a
    for row in r[first:first + n]:
        if has_uv:
            u, v = dec_uv(m, row, a["uv0"])
            lines.append(f"vt {u:.6f} {1 - v:.6f}")
    for row in r[first:first + n]:
        if has_n:
            lines.append("vn {:.4f} {:.4f} {:.4f}".format(*dec_vec(row, a["normal"])))
    for t in tris:
        k = [i - first + 1 for i in t]
        if min(k) < 1 or max(k) > n:
            raise MeshError(f"submesh {sm.index}: a triangle uses a vertex outside its range")
        if has_uv and has_n:
            lines.append("f " + " ".join(f"{i}/{i}/{i}" for i in k))
        elif has_n:
            lines.append("f " + " ".join(f"{i}//{i}" for i in k))
        else:
            lines.append("f " + " ".join(str(i) for i in k))
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")
    return n, len(tris)


def skin_json(m: Mesh, sm: Submesh) -> dict:
    a = m.attrs()
    vb, _, _ = m.main
    r = rows(m, vb)[sm.first_vertex:sm.first_vertex + sm.vertex_count]
    out = {"bones": [{"hash": f"{h:08x}", "matrix": list(mat)} for h, mat in m.bones], "bone_map": sm.bone_map}
    if "bones" in a:
        out["vertices"] = [[[x[a["bones"] + i] for i in range(4)], [x[a["weights"] + i] for i in range(4)]]
                           for x in r]
    return out


def cmd_export(a):
    s = load_source(a.src, a.entry)
    if a.out.lower().endswith(".glb"):
        res = mesh_resources(s, a.mesh)
        extra: dict[int, list] = {}
        for spec in a.rig or []:
            if ".forge:" in spec:
                arch, ent = spec.split(".forge:", 1)
                arch += ".forge"
            elif a.src.endswith(".forge"):
                arch, ent = a.src, spec
            else:
                sys.exit(f"--rig {spec}: give ARCHIVE.forge:ENTRY (the source is not an archive)")
            for _, r in mesh_resources(load_source(arch, ent), None):
                skinned_rest(parse_mesh(r.payload), extra)
        archive = a.textures if isinstance(a.textures, str) else a.src if a.src.endswith(".forge") else None
        n, nb = export_glb(s, res, a.out, bool(a.textures), archive, extra)
        print(f"{a.out}: {n} mesh(es)" + (f", armature of {nb} bones" if nb else ""))
        return
    names = names_by_id(s)
    os.makedirs(a.out, exist_ok=True)
    for _, r in mesh_resources(s, a.mesh):
        m = parse_mesh(r.payload)
        for sm in m.submeshes:
            base = os.path.join(a.out, f"{forge.safe_name(r.name)}_sm{sm.index}")
            nv, nt = export_obj(m, r.name, sm, base + ".obj", names)
            if a.skin and m.bones:
                with open(base + ".skin.json", "w") as f:
                    json.dump(skin_json(m, sm), f)
            print(f"{base}.obj: {nv} vertices, {nt} triangles")


# --- edit -------------------------------------------------------------------------------------------------------

def rewrite(m: Mesh, new_main: list[bytes], new_S: float | None = None) -> bytes:
    """The payload with the main buffer's rows replaced (same count), the section-2 copy kept in step, the
    bounds recomputed where positions moved and the position scale updated. Unchanged rows keep their bytes."""
    vb, _, _ = m.main
    old = rows(m, vb)
    if len(new_main) != len(old):
        raise MeshError("vertex count changed (use import for new topology)")
    if new_main == old and new_S is None:
        return m.data
    p = bytearray(m.data)
    p[vb.off:vb.off + vb.size] = b"".join(new_main)
    if m.s1 and m.s1["vb"].size and m.vb2.size:  # section 2: the same rows, reordered
        index: dict[bytes, list[int]] = {}
        for i, row in enumerate(old):
            index.setdefault(row, []).append(i)
        used: dict[bytes, int] = {}
        out = []
        for row in rows(m, m.vb2):
            cand = index.get(row)
            if not cand:
                raise MeshError("section 2 holds a vertex that is not in section 1")
            k = used.get(row, 0)
            used[row] = k + 1
            out.append(new_main[cand[min(k, len(cand) - 1)]])
        p[m.vb2.off:m.vb2.off + m.vb2.size] = b"".join(out)
    if new_S is not None:
        struct.pack_into("<f", p, m.scale_off, new_S)
    m2 = parse_mesh(bytes(p))
    pts = [dec_pos(m2, x) for x in new_main]
    if pts and [dec_pos(m, x) for x in old] != pts:
        lo, hi = bounds(pts)
        struct.pack_into("<8f", p, m.bbox_off, *lo, 0.0, *hi, 0.0)
        if m.s1:
            c = m.s1["center_off"]
            p[c:c + 24] = center_half(lo, hi)
            for k in range(m.s1["n"]):
                fv, _, vc, *_ = m.prims[0][k]
                if vc:
                    c = m.s1["recs_off"] + REC_SIZE * k
                    p[c:c + 24] = center_half(*bounds(pts[fv:fv + vc]))
    return bytes(p)


def scale_for(m: Mesh, pts) -> float | None:
    """A new position scale when `pts` do not fit the current one (S is a whole number of metres in the data)."""
    need = max((abs(c) for p in pts for c in p), default=0.0)
    cur = m.S if m.fmt in SKINNED else max(abs(struct.unpack_from("<h", m.data, m.main[0].off + 6)[0]), 1)
    if need <= cur * (1 + 0.4 / Q):  # rounds to at most +-32767
        return None
    return float(math.ceil(need * 1.001))


def apply_positions(m: Mesh, new_pts: list) -> bytes:
    """Encode new positions into the main rows (keeping every other byte) and rewrite the payload."""
    vb, _, _ = m.main
    old = rows(m, vb)
    S2 = scale_for(m, new_pts)
    if S2 is not None and m.fmt not in SKINNED and S2 > Q:
        raise MeshError("positions too large for a static mesh's scale")
    out = []
    for row, pt in zip(old, new_pts):
        if S2 is None and pt == dec_pos(m, row):
            out.append(row)
        else:
            out.append(enc_pos(m, row, pt, S2))
    return rewrite(m, out, S2)


def welded_normals(m: Mesh, r: list[bytes]) -> list[tuple]:
    """Each vertex's normal averaged over all vertices at the same position, so seams move together. Where the
    copies face apart (both sides of a thin sheet: the average is short), each keeps its own normal."""
    a = m.attrs()
    if "normal" not in a:
        raise MeshError(f"format {m.fmt} has no normals")
    acc: dict[bytes, list[float]] = {}
    for row in r:
        n = norm(dec_vec(row, a["normal"]))
        s = acc.setdefault(row[:6], [0.0, 0.0, 0.0, 0])
        for i in range(3):
            s[i] += n[i]
        s[3] += 1
    out = []
    for row in r:
        s = acc[row[:6]]
        avg = [c / s[3] for c in s[:3]]
        out.append(norm(avg) if math.sqrt(dot(avg, avg)) > 0.5 else norm(dec_vec(row, a["normal"])))
    return out


def transform(m: Mesh, args) -> list[tuple]:
    vb, _, _ = m.main
    r = rows(m, vb)
    pts = [dec_pos(m, x) for x in r]
    if args.inflate:
        ns = welded_normals(m, r) if not args.split_normals else [norm(dec_vec(x, m.attrs()["normal"])) for x in r]
        pts = [tuple(c + args.inflate * n for c, n in zip(p, nn)) for p, nn in zip(pts, ns)]
    if args.scale:
        sc = [float(x) for x in args.scale.split(",")]
        sc = sc * 3 if len(sc) == 1 else sc
        if args.pivot:
            piv = [float(x) for x in args.pivot.split(",")]
        else:
            bb = struct.unpack_from("<8f", m.data, m.bbox_off)
            piv = [(bb[i] + bb[4 + i]) / 2 for i in range(3)]
        pts = [tuple(pv + (c - pv) * s for c, pv, s in zip(p, piv, sc)) for p in pts]
    if args.translate:
        t = [float(x) for x in args.translate.split(",")]
        pts = [tuple(c + d for c, d in zip(p, t)) for p in pts]
    return pts


def save(s: Source, res: list, out: str, more: dict) -> str:
    base = os.path.join(out, f"{s.entry:016x}_{forge.safe_name(s.name)}")
    extra = {k: v for k, v in (s.manifest or {}).items() if k in ("prefetch", "swap")}
    forge.write_unpacked(base, s.entry, s.name, s.type, res, **extra, **more)
    check = forge.parse_datafile(forge.build_datafile(res))
    assert [x.payload for x in check.resources] == [x.payload for x in res]
    return base


def cmd_edit(a):
    s = load_source(a.src, a.entry)
    res = list(s.resources)
    done = []
    for k, r in mesh_resources(s, a.mesh):
        m = parse_mesh(r.payload)
        new = apply_positions(m, transform(m, a))
        if new != r.payload:
            parse_mesh(new)
        res[k] = forge.Resource(r.id, r.type, r.name, new, r.flag, r.pre, r.extra)
        same = "unchanged" if new == r.payload else "changed"
        print(f"  {r.name}: {m.vertex_count} vertices, payload {same}")
        done.append(r.name)
    what = {"inflate": a.inflate, "scale": a.scale, "pivot": a.pivot, "translate": a.translate}
    base = save(s, res, a.out, {"mesh_edit": {"meshes": done, **{k: v for k, v in what.items() if v}}})
    print(f"{base}/: {s.name} with {len(done)} edited mesh(es)")


# --- import -----------------------------------------------------------------------------------------------------

def read_obj(path: str) -> dict:
    v, vt, vn, faces, name = [], [], [], [], None
    with open(path) as f:
        for line in f:
            t = line.split()
            if not t:
                continue
            if t[0] == "v":
                v.append(tuple(float(x) for x in t[1:4]))
            elif t[0] == "vt":
                vt.append(tuple(float(x) for x in t[1:3]))
            elif t[0] == "vn":
                vn.append(tuple(float(x) for x in t[1:4]))
            elif t[0] == "f":
                corners = []
                for c in t[1:]:
                    parts = (c.split("/") + ["", ""])[:3]
                    idx = []
                    for pp, lst in zip(parts, (v, vt, vn)):
                        if pp:
                            k = int(pp)
                            idx.append(k - 1 if k > 0 else len(lst) + k)
                        else:
                            idx.append(None)
                    corners.append(tuple(idx))
                for i in range(1, len(corners) - 1):  # fan
                    faces.append((corners[0], corners[i], corners[i + 1]))
            elif t[0] in ("o", "g") and len(t) > 1 and name is None:
                name = t[1]
    return {"v": v, "vt": vt, "vn": vn, "f": faces, "name": name}


def match_obj(path: str, obj: dict, meshes: dict[str, tuple]) -> tuple[str, int]:
    for label in (obj["name"], os.path.splitext(os.path.basename(path))[0]):
        mm = re.fullmatch(r"(.+)_sm(\d+)", label or "")
        if mm:
            for name in meshes:
                if forge.safe_name(name) == mm[1] or name == mm[1]:
                    return name, int(mm[2])
    sys.exit(f"{path}: name it (or its `o` line) <mesh>_sm<k> as export does; meshes here: {', '.join(meshes)}")


def import_same(m: Mesh, sm: Submesh, obj: dict, rows_: list[bytes]) -> int:
    """Same topology: OBJ vertex i is submesh vertex i. Positions, UVs and normals replace the stored ones where
    they differ by more than the quantisation; tangent frames follow a changed normal. Returns rows changed."""
    a = m.attrs()
    n = sm.vertex_count
    if len(obj["v"]) != n:
        raise MeshError(f"submesh {sm.index}: the OBJ has {len(obj['v'])} vertices, the mesh {n} "
                        "(use --topology for a different mesh)")
    want = sorted(tuple(sorted(i - sm.first_vertex for i in t)) for t in triangles(m, sm))
    have = sorted(tuple(sorted(c[0] for c in f)) for f in obj["f"])
    if want != have:
        raise MeshError(f"submesh {sm.index}: the OBJ's faces differ from the mesh's (use --topology)")
    uv = [None] * n
    nv = [None] * n
    for f in obj["f"]:
        for vi, ti, ni in f:
            if ti is not None and uv[vi] is None:
                uv[vi] = obj["vt"][ti]
            if ni is not None and nv[vi] is None:
                nv[vi] = obj["vn"][ni]
    changed = 0
    S2 = scale_for(m, obj["v"])
    if S2 is not None:
        raise MeshError(f"submesh {sm.index}: positions leave the +-{m.S} m range (not supported by import yet)")
    for i in range(n):
        k = sm.first_vertex + i
        row = rows_[k]
        new = enc_pos(m, row, obj["v"][i])  # the same bytes when the OBJ's value rounds to the stored one
        if uv[i] is not None and "uv0" in a:
            u, v = uv[i][0], 1 - uv[i][1]
            q = (round(u * Q / m.X), round(v * Q / m.X))
            if q != struct.unpack_from("<2h", new, a["uv0"]):
                if max(abs(c) for c in q) > Q:
                    raise MeshError(f"submesh {sm.index}: UV {uv[i]} is outside the mesh's UV range +-{m.X:.4f}")
                new = new[:a["uv0"]] + struct.pack("<2h", *q) + new[a["uv0"] + 4:]
        if nv[i] is not None and "normal" in a:
            old_n = norm(dec_vec(new, a["normal"]))
            n_new = norm(nv[i])
            if dot(old_n, n_new) < math.cos(math.radians(1.0)):
                o = a["normal"]
                new = new[:o] + enc_vec(n_new) + new[o + 3:]
                for t in ("tangent", "binormal"):  # Gram-Schmidt against the new normal, handedness kept
                    if t in a:
                        tv = dec_vec(new, a[t])
                        tv = norm(tuple(c - n_new[j] * dot(tv, n_new) for j, c in enumerate(tv)))
                        new = new[:a[t]] + enc_vec(tv) + new[a[t] + 3:]
        if new != row:
            rows_[k] = new
            changed += 1
    return changed


class Nearest:
    """Nearest stored vertex to a point (uniform grid; pure Python)."""

    def __init__(self, pts: list):
        lo, hi = bounds(pts)
        self.cell = max(max(b - a for a, b in zip(lo, hi)) / max(len(pts) ** (1 / 3), 1), 1e-4)
        self.grid: dict[tuple, list[int]] = {}
        self.pts = pts
        for i, p in enumerate(pts):
            self.grid.setdefault(self.key(p), []).append(i)

    def key(self, p):
        return tuple(int(math.floor(c / self.cell)) for c in p)

    def __call__(self, p) -> int:
        k = self.key(p)
        best, bd, r = None, float("inf"), 0
        while best is None or r * self.cell < math.sqrt(bd) + self.cell:
            for dx in range(-r, r + 1):
                for dy in range(-r, r + 1):
                    for dz in range(-r, r + 1):
                        if max(abs(dx), abs(dy), abs(dz)) != r:
                            continue
                        for i in self.grid.get((k[0] + dx, k[1] + dy, k[2] + dz), ()):
                            q = self.pts[i]
                            d = (q[0] - p[0]) ** 2 + (q[1] - p[1]) ** 2 + (q[2] - p[2]) ** 2
                            if d < bd:
                                best, bd = i, d
            r += 1
            if r > 64 and best is not None:
                break
        return best


@dataclass
class Vtx:
    """A vertex of a rebuilt submesh. What it leaves as None comes from the nearest original vertex."""
    pos: tuple
    uv: tuple = (0.0, 0.0)          # as stored: v pointing down
    normal: tuple | None = None
    uv1: tuple | None = None
    skin: list | None = None        # [(index into the mesh's bone list, weight)]: any number, any scale
    color: bytes | None = None      # format 6's u8x4
    row: bytes | None = None        # the stored vertex this one is (`--add`): its bytes are kept


def obj_geometry(obj: dict) -> tuple[list[Vtx], list[tuple]]:
    """One vertex per distinct (v, vt, vn) corner of an OBJ, and the triangles over them."""
    corner: dict[tuple, int] = {}
    verts, tris = [], []
    for f in obj["f"]:
        t = []
        for c in f:
            if c not in corner:
                corner[c] = len(verts)
                uv = obj["vt"][c[1]] if c[1] is not None else (0.0, 0.0)
                verts.append(Vtx(obj["v"][c[0]], (uv[0], 1 - uv[1]), obj["vn"][c[2]] if c[2] is not None else None))
            t.append(corner[c])
        tris.append(tuple(t))
    return verts, tris


def cross(a, b):
    return (a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0])


def frames(verts: list[Vtx], tris: list[tuple]) -> list[tuple]:
    """Per vertex (normal, tangent, binormal): the normal given, or area-weighted from the faces at that position;
    tangent = dP/du, binormal = dP/d(1 - v) (what the shipped data holds, v pointing down), summed over the faces
    (zero where the UVs give none)."""
    fn: dict[tuple, list[float]] = {}
    tg = [[0.0] * 3 for _ in verts]
    bn = [[0.0] * 3 for _ in verts]
    for t in tris:
        (P0, U0), (P1, U1), (P2, U2) = ((verts[i].pos, verts[i].uv) for i in t)
        e1 = [P1[k] - P0[k] for k in range(3)]
        e2 = [P2[k] - P0[k] for k in range(3)]
        f = cross(e1, e2)
        for i in t:
            acc = fn.setdefault(verts[i].pos, [0.0, 0.0, 0.0])
            for k in range(3):
                acc[k] += f[k]
        du1, dv1, du2, dv2 = U1[0] - U0[0], U1[1] - U0[1], U2[0] - U0[0], U2[1] - U0[1]
        det = du1 * dv2 - du2 * dv1
        if abs(det) < 1e-12:
            continue
        for i in t:
            for k in range(3):
                tg[i][k] += (e1[k] * dv2 - e2[k] * dv1) / det
                bn[i][k] += -(e2[k] * du1 - e1[k] * du2) / det
    return [(norm(v.normal if v.normal is not None else fn.get(v.pos, (0.0, 0.0, 1.0))), tg[k], bn[k])
            for k, v in enumerate(verts)]


def topology_rows(m: Mesh, sm: Submesh, verts: list[Vtx], tris: list[tuple], all_rows: list[bytes]
                  ) -> tuple[list, list]:
    """For new vertices of a submesh: the nearest stored vertex of the same submesh, which lends what a vertex does
    not carry (skinning, uv1, the position's w, the other bytes), and each vertex's tangent frame (n, t, b) with t
    and b made perpendicular to n (where the UVs give no gradient: the nearest vertex's)."""
    a = m.attrs()
    own = all_rows[sm.first_vertex:sm.first_vertex + sm.vertex_count]
    if not own:
        raise MeshError(f"submesh {sm.index} has no vertices to take the other attributes from")
    near = Nearest([dec_pos(m, x) for x in own])
    step = (m.S if m.fmt in SKINNED else abs(struct.unpack_from("<h", own[0], 6)[0])) / Q
    at_pos: dict[tuple, list[bytes]] = {}  # seams: several vertices share a position; take the one that matches
    for x in own:
        at_pos.setdefault(tuple(round(c / step) for c in dec_pos(m, x)), []).append(x)
    templates, out = [], []
    for v, (n, tg, bn) in zip(verts, frames(verts, tris)):
        cand = at_pos.get(tuple(round(c / step) for c in v.pos))
        if v.row is not None:
            tpl = v.row
        elif cand:
            tn = norm(tg) if dot(tg, tg) > 1e-20 else (0.0, 0.0, 0.0)
            tpl = min(cand, key=lambda x: sum(abs(c - d) for c, d in zip(v.uv, dec_uv(m, x, a["uv0"])))
                      - dot(n, norm(dec_vec(x, a["normal"]))) - 0.1 * dot(tn, norm(dec_vec(x, a["tangent"]))))
        else:
            tpl = own[near(v.pos)]
        if dot(tg, tg) < 1e-20:
            tg = dec_vec(tpl, a["tangent"])
        if dot(bn, bn) < 1e-20:
            bn = dec_vec(tpl, a["binormal"]) if "binormal" in a else cross(n, tg)
        t2 = norm(tuple(c - n[j] * dot(tg, n) for j, c in enumerate(tg)))
        b2 = tuple(c - n[j] * dot(bn, n) - t2[j] * dot(bn, t2) for j, c in enumerate(bn))
        b2 = norm(b2) if dot(b2, b2) > 1e-20 else norm(cross(n, t2))
        templates.append(tpl)
        out.append((n, t2, b2))
    return templates, out


def quantise_skin(skin: list, bone_map: list) -> tuple[bytes, bytes] | None:
    """Up to 4 influences, heaviest first, as u8 slots into `bone_map` (bones it lacks are appended) and u8 weights
    summing to 255. Unused slots are 0 / 0, as in the shipped data. None if no weight is left."""
    tot: dict[int, float] = {}
    for b, w in skin:
        if w > 0:
            tot[b] = tot.get(b, 0.0) + w
    top = sorted(tot.items(), key=lambda kv: -kv[1])[:4]
    s = sum(w for _, w in top)
    if not top or s <= 0:
        return None
    raw = [w * 255 / s for _, w in top]
    q = [int(x) for x in raw]
    for i in sorted(range(len(q)), key=lambda i: q[i] - raw[i])[:255 - sum(q)]:  # largest remainders
        q[i] += 1
    pairs = [(b, w) for (b, _), w in zip(top, q) if w]
    slots = []
    for b, _ in pairs:
        if b not in bone_map:
            if len(bone_map) >= 128:
                raise MeshError("a submesh's bone map holds at most 128 bones")
            bone_map.append(b)
        slots.append(bone_map.index(b))
    pad = [0] * (4 - len(pairs))
    return bytes(slots + pad), bytes([w for _, w in pairs] + pad)


def splice(p: bytes, edits: list[tuple[int, int, bytes]]) -> bytes:
    out, pos = [], 0
    for start, end, data in sorted(edits):
        if start < pos:
            raise MeshError("overlapping edits")
        out += [p[pos:start], data]
        pos = end
    return b"".join(out + [p[pos:]])


def rebuild(m: Mesh, subs: dict[int, tuple[list[bytes], list[tuple]]], bone_maps: dict[int, list] | None = None
            ) -> bytes:
    """A payload with new vertex/index buffers: `subs` maps each submesh to (rows, local triangles). Rewrites
    section 1 (offsets, buffers, per-submesh bounds and index ranges), section 2's draw ranges (and its copy of
    the buffers when it has one), the submesh table's vertex counts and bone maps (`bone_maps`) and the bounds;
    everything else is kept."""
    order = [sm.index for sm in m.submeshes]
    if sorted(order) != list(range(len(m.prims[0]))):
        raise MeshError("the submesh table does not cover the draw ranges 0..n-1")
    allrows, ib_local, ib_abs, ranges = [], [], [], []
    for k in range(len(m.prims[0])):
        r, t = subs[k]
        if len(r) > 0xFFFF:
            raise MeshError(f"submesh {k}: {len(r)} vertices (16-bit indices allow 65535)")
        if any(i < 0 or i >= len(r) for tri in t for i in tri):
            raise MeshError(f"submesh {k}: a triangle uses a vertex of another submesh")
        fv, fi = len(allrows), len(ib_local)
        allrows += r
        for tri in t:
            ib_local += tri
            ib_abs += [fv + i for i in tri]
        ranges.append((fv, len(r), fi, len(t)))
    nv, total_tris = len(allrows), len(ib_abs) // 3
    p = m.data
    edits = []
    pts = [dec_pos(m, x) for x in allrows]
    lo, hi = bounds(pts)
    edits.append((m.bbox_off, m.bbox_off + 32, struct.pack("<8f", *lo, 0.0, *hi, 0.0)))
    vb_bytes = b"".join(allrows)
    two = m.vb2.size > 0
    if m.s1 and m.s1["vb"].size:
        s1 = m.s1
        n = s1["n"]
        edits.append((s1["center_off"], s1["center_off"] + 24, center_half(lo, hi)))
        edits.append((s1["a2_off"], s1["a2_off"] + 4 * n,
                      struct.pack(f"<{n}I", *[fv * m.stride // 4 for fv, *_ in ranges])))
        vb, ib = s1["vb"], s1["ib"]
        edits.append((vb.off - 4, vb.off + vb.size, struct.pack("<I", len(vb_bytes)) + vb_bytes))
        ibb = struct.pack(f"<{len(ib_local)}H", *ib_local)
        edits.append((ib.off - 4, ib.off + ib.size, struct.pack("<I", len(ibb)) + ibb))
        recs = bytearray(p[s1["recs_off"]:s1["recs_off"] + REC_SIZE * n])
        for k, (fv, vc, fi, tc) in enumerate(ranges):
            recs[REC_SIZE * k:REC_SIZE * k + 24] = center_half(*(bounds(pts[fv:fv + vc]) if vc else (lo, lo)))
            flag = recs[REC_SIZE * k + 28]
            struct.pack_into("<II", recs, REC_SIZE * k + 28, (3 * tc) << 8 | flag, fi)
        edits.append((s1["recs_off"], s1["recs_off"] + len(recs), bytes(recs)))
    else:
        two = True  # section 2 is the only copy
    if two:
        if nv > 0xFFFF and not m.idx32:
            raise MeshError(f"{nv} vertices need 32-bit indices in section 2")
        ib2 = struct.pack(f"<{len(ib_abs)}{'I' if m.idx32 else 'H'}", *ib_abs)
        edits.append((m.vb2.off - 4, m.vb2.off + m.vb2.size, struct.pack("<I", len(vb_bytes)) + vb_bytes))
        edits.append((m.ib2.off - 4, m.ib2.off + m.ib2.size, struct.pack("<I", len(ib2)) + ib2))
    # The second draw list is, in the shipped data: a copy of the first, a subset of it (some submeshes left
    # out, G: those that cast no shadow), or one range over all of them; other forms (24 of 694 sampled) refuse.
    new0 = [(fv, 0, vc, fi, tc, m.prims[0][k][5]) for k, (fv, vc, fi, tc) in enumerate(ranges)]
    p0, p1 = m.prims
    if all(x in p0 for x in p1):
        new1 = [new0[p0.index(x)] for x in p1]
    elif (len(p1) == 1 and p1[0][0] == p1[0][1] == p1[0][3] == 0 and p1[0][4] == sum(x[4] for x in p0)
          and p1[0][2] in (m.vertex_count, sum(x[2] for x in p0))):
        new1 = [(0, 0, nv, 0, total_tris, p1[0][5])]
    else:
        raise MeshError("section 2's second draw list is not a copy, subset or merged range of the first")
    for lst, offs in ((new0, m.prims_off[0]), (new1, m.prims_off[1])):
        for rec, off in zip(lst, offs):
            edits.append((off, off + 24, struct.pack("<6I", *rec)))
    for sm in m.submeshes:
        body = sm.table_off
        if body is None:
            continue
        old = int.from_bytes(p[body + 7:body + 10], "little")  # u24: the vertex count where it held the old one (G)
        if old == sm.vertex_count:
            edits.append((body + 7, body + 10, ranges[sm.index][1].to_bytes(3, "little")))
        bm = (bone_maps or {}).get(sm.index)
        if bm is not None and bm != sm.bone_map:  # u16 length at +3, u16[128] at +18 (unused 0xF7F7)
            edits.append((body + 3, body + 5, struct.pack("<H", len(bm))))
            edits.append((body + 18, body + 18 + 256, struct.pack("<128H", *bm, *[0xF7F7] * (128 - len(bm)))))
    return splice(p, edits)


def add_bones(p: bytes, bones: list[tuple[int, list]]) -> bytes:
    """The payload with (name hash, inverse bind matrix) bones appended to its bone list."""
    n = struct.unpack_from("<I", p, 0x17)[0]
    end = 0x17 + 4 + BONE_SIZE * n
    recs = b"".join(struct.pack("<QII16fB", 0, CLS_BONE, h, *mat, 0) for h, mat in bones)
    return p[:0x17] + struct.pack("<I", n + len(bones)) + p[0x1B:end] + recs + p[end:]


TOPOLOGY_FORMATS = (0, 2, 6, 8)


def import_topology(m: Mesh, items: list[tuple[Submesh, list[Vtx], list[tuple]]]) -> tuple[bytes, dict]:
    """Submeshes replaced by new geometry of any topology: skinned formats 0 and 2 (explicit binormal), static 6 and
    8 (the position's w sign is the frame's handedness). Returns the payload and per submesh how many vertices took
    their skinning from the nearest original vertex."""
    if m.fmt not in TOPOLOGY_FORMATS:
        raise MeshError(f"new topology supports vertex formats {TOPOLOGY_FORMATS}, not {m.fmt}")
    a = m.attrs()
    skinned = m.fmt in SKINNED
    vb, _, _ = m.main
    allr = rows(m, vb)
    built = {}
    for sm, verts, tris in items:
        built[sm.index] = (verts, tris, *topology_rows(m, sm, verts, tris, allr))
    keep = {sm.index: (allr[sm.first_vertex:sm.first_vertex + sm.vertex_count], triangles(m, sm))
            for sm in m.submeshes if sm.index not in built}
    pts = [dec_pos(m, x) for r, _ in keep.values() for x in r] + [v.pos for vs, *_ in built.values() for v in vs]
    cur = m.S if skinned else abs(struct.unpack_from("<h", allr[0], 6)[0]) if allr else m.S
    S2 = scale_for(m, pts) or cur
    if not skinned and S2 > Q:
        raise MeshError("positions too large for a static mesh's scale")
    uvk = [k for k in ("uv0", "uv1") if k in a]
    uvs = [abs(c) for r, _ in keep.values() for x in r for k in uvk for c in dec_uv(m, x, a[k])]
    for vs, _, tpl, _ in built.values():
        uvs += [abs(c) for v in vs for c in v.uv]
        if "uv1" in a:
            uvs += [abs(c) for v, x in zip(vs, tpl) for c in (v.uv1 if v.uv1 is not None else dec_uv(m, x, a["uv1"]))]
    X2 = max(max(uvs, default=0.0), m.X)
    # a submesh rebuilt only from vertices with their own weights starts with an empty bone map (its old slots are
    # unused, and appending to a long map could pass 128); otherwise new bones are appended to the old map
    fresh = {i for i, (vs, *_) in built.items() if vs and all(v.skin and v.row is None for v in vs)}
    maps = {sm.index: [] if sm.index in fresh else list(sm.bone_map) for sm in m.submeshes}
    fallback: dict[int, int] = {}

    def skin_set(row: bytes, bm: list, off: int | None = None) -> list:
        """(bone, weight) pairs of a row's bones/weights bytes (or of 8 bytes slots + weights at `off`)."""
        o_b, o_w = (a["bones"], a["weights"]) if off is None else (off, off + 4)
        return sorted((bm[row[o_b + i]] if row[o_b + i] < len(bm) else -1 - row[o_b + i], row[o_w + i])
                      for i in range(4) if row[o_w + i])

    def same_as(row: bytes, v: Vtx) -> bool:
        """`v` is the template vertex itself (as exported): it keeps the template's frame and skin bytes."""
        if max(abs(c - d) for c, d in zip(v.pos, dec_pos(m, row))) > 0.5 * cur / Q:
            return False
        if max(abs(c - d) for c, d in zip(v.uv, dec_uv(m, row, a["uv0"]))) > 0.5 * m.X / Q:
            return False
        return v.normal is not None and dot(norm(v.normal), norm(dec_vec(row, a["normal"]))) > 1 - 1e-6

    def enc(row: bytes, sub: int, v: Vtx | None = None, fr=None) -> bytes:
        tpl = row
        row = bytearray(row)
        same = v is not None and (v.row is not None or same_as(tpl, v))
        if v is not None and not skinned and not same:  # handedness: w > 0 when cross(n, t) points along b
            n, t, b = fr
            row[6:8] = struct.pack("<h", 1 if dot(cross(n, t), b) > 0 else -1)
        row = bytearray(enc_pos(m, bytes(row), v.pos if v is not None else dec_pos(m, bytes(row)), S2))
        for k in uvk if X2 > 0 else ():  # X = 0: the mesh has no UVs
            if v is not None and k == "uv0":
                u = v.uv
            elif v is not None and v.uv1 is not None:
                u = v.uv1
            else:
                u = dec_uv(m, bytes(row), a[k])
            struct.pack_into("<2h", row, a[k], *[max(-Q, min(Q, round(c * Q / X2))) for c in u])
        if v is not None:
            for vec, k in zip(fr, ("normal", "tangent", "binormal")):
                if k in a and not same:
                    row[a[k]:a[k] + 3] = enc_vec(vec)
            if v.color is not None and "color" in a:
                row[a["color"]:a["color"] + 4] = v.color
            if "weights" in a:
                q = quantise_skin(v.skin, maps[sub]) if v.skin else None
                if q is None:
                    fallback[sub] = fallback.get(sub, 0) + 1
                elif not (same and skin_set(tpl, maps[sub]) == skin_set(q[0] + q[1], maps[sub], 0)):
                    row[a["bones"]:a["bones"] + 4], row[a["weights"]:a["weights"] + 4] = q
        return bytes(row)

    rescale = S2 != cur or X2 != m.X
    subs = {}
    for sm in m.submeshes:
        if sm.index in built:
            verts, tris, tpl, frs = built[sm.index]
            subs[sm.index] = ([enc(row, sm.index, v, fr) for row, v, fr in zip(tpl, verts, frs)], tris)
        else:
            r, t = keep[sm.index]
            subs[sm.index] = ([enc(x, sm.index) for x in r] if rescale else r,
                              [tuple(i - sm.first_vertex for i in tri) for tri in t])
    m2 = Mesh(**{**m.__dict__, "S": S2, "X": X2})
    p = bytearray(rebuild(m2, subs, maps))
    struct.pack_into("<2f", p, parse_mesh(bytes(p)).scale_off, m.S if S2 == cur else S2, X2)
    return bytes(p), fallback


# --- glTF ---------------------------------------------------------------------------------------------------------

def material_label(sm: Submesh, m: Mesh, names: dict[int, str]) -> str:
    """The glTF material name of a submesh: its material's name, plus `#sm<k>` where several submeshes share it."""
    base = names.get(sm.material) or f"0x{sm.material:x}"
    if sum(1 for x in m.submeshes if x.material == sm.material) > 1:
        base += f"#sm{sm.index}"
    return base


def guess_parents(rest: dict[int, list]) -> dict[int, int | None]:
    """A parent per bone, for posing in Blender only: PARENTS where both are present (walking up past missing
    bones), else the nearest bone that has a place in PARENTS (by bind-pose origin)."""
    present = set(rest)
    ref = zlib.crc32(b"Reference")

    def up(name: str) -> int | None:
        while name in PARENTS:
            name = PARENTS[name]
            if zlib.crc32(name.encode()) in present:
                return zlib.crc32(name.encode())
        return None
    anchors = [h for h in present if bone_name(h) in PARENTS or h == ref]
    out = {}
    for h in present:
        core = re.sub(r"^(L|O|P|RFX)_|(_NoRoll|Roll|Bend|_Upnodes?|Back)$", "", bone_name(h))
        if bone_name(h) in PARENTS:
            out[h] = up(bone_name(h))
        elif core != bone_name(h) and core in PARENTS:  # a helper: follows its bone (or the nearest one up)
            out[h] = zlib.crc32(core.encode()) if zlib.crc32(core.encode()) in present else up(core)
        elif h == ref or not anchors:
            out[h] = None
        else:
            o = rest[h][12:15]
            out[h] = min((x for x in anchors if x != h),
                         key=lambda x: sum((c - d) ** 2 for c, d in zip(rest[x][12:15], o)), default=None)
    return out


def diffuse_png(s: Source, mat_id: int, archive: str | None, cache: dict) -> tuple[bytes, str] | None:
    """The material's diffuse texture (TextureSet slot 0) as PNG in storage order, row 0 at v = 0 as the game samples
    it (seam colours agree that way; texture.py export flips rows, which is right for UI textures only): the top
    level stored inline in its TextureMap (Mip levels are not fetched). Needs numpy; None where it cannot be found."""
    try:
        import texture
    except ImportError:
        print("  (textures need numpy: left out)", file=sys.stderr)
        return None
    by_id = {r.id: r for r in s.resources}
    mat = by_id.get(mat_id)
    if mat is None:
        return None
    tex_id = None
    for r in s.resources:
        if r.type == 0xD70E6670 and struct.pack("<Q", r.id) in mat.payload and r.payload[13] == 1:
            tex_id = struct.unpack_from("<Q", r.payload, 15)[0]
            break
    if not tex_id:
        return None
    res = by_id.get(tex_id)
    if res is None and archive:
        hit = texture.find_mip_entry(tex_id, archive, cache, anywhere=True)
        if hit:
            res = next((r for r in forge.parse_datafile(hit[0].read(hit[1])).resources if r.id == tex_id), None)
    if res is None:
        return None
    t = texture.parse_texture(res.payload)
    if t.fmt not in texture.FORMATS or t.dim != 1:
        return None
    w, h, _ = texture.level_dims(t.width, t.height, t.depth, t.first_level)
    img = texture.decode_level(t.fmt, t.data, w, h)
    return texture.png_bytes(img[:, :, :3]), res.name


def skinned_rest(m: Mesh, rest: dict[int, list]):
    """Each bone's rest pose (glTF space) from a skinned mesh's inverse bind matrices, where `rest` has none yet."""
    if "weights" in m.attrs():
        for h, M in m.bones:
            rest.setdefault(h, gltf.mat_to_gl(gltf.mat_inv_rigid(M)))


def export_glb(s: Source, res: list, path: str, textures: bool, archive: str | None, extra: dict | None = None):
    """One .glb: a node per mesh (LOD) with a primitive per submesh, and for skinned meshes an armature
    (Steep_Rig) whose bones sit at the inverse bind matrices' rest poses, with the per-vertex weights. `extra`
    (hash -> rest pose) adds bones that no exported mesh has, in a skin of their own (Steep_Rig_extra) so that
    Blender makes them bones of the same armature."""
    names = names_by_id(s)
    b = gltf.Builder("powder tools/forge/mesh.py")
    doc = b.doc
    meshes = [(r, parse_mesh(r.payload)) for _, r in res]
    rest: dict[int, list] = {}
    for _, m in meshes:
        skinned_rest(m, rest)
    only_extra = [h for h in (extra or {}) if h not in rest]
    for h in only_extra:
        rest[h] = extra[h]
    joint, rig = {}, None
    if rest:
        rig = b.add("nodes", {"name": "Steep_Rig"})
        doc["scenes"][0]["nodes"].append(rig)
        parent = guess_parents(rest)
        for h in rest:
            joint[h] = b.add("nodes", {"name": bone_name(h)})
        for h, M in rest.items():
            p = parent[h]
            local = M if p is None else gltf.mat_mul(gltf.mat_inv_rigid(rest[p]), M)
            doc["nodes"][joint[h]].update(gltf.trs(local))
            doc["nodes"][joint[p] if p is not None else rig].setdefault("children", []).append(joint[h])
        if only_extra:
            ibm = b.accessor([gltf.mat_inv_rigid(rest[h]) for h in only_extra], "MAT4", target=None)
            b.add("skins", {"name": "Steep_Rig_extra", "joints": [joint[h] for h in only_extra],
                            "inverseBindMatrices": ibm, "skeleton": rig})
    mats: dict[str, int] = {}
    images: dict[int, int | None] = {}
    cache: dict = {}
    for r, m in meshes:
        a = m.attrs()
        allr = rows(m, m.main[0])
        skin = None
        if "weights" in a and m.bones:
            ibm = b.accessor([gltf.mat_to_gl(M) for _, M in m.bones], "MAT4", target=None)
            skin = b.add("skins", {"name": r.name, "joints": [joint[h] for h, _ in m.bones],
                                   "inverseBindMatrices": ibm, "skeleton": rig})
        prims = []
        for sm in m.submeshes:
            rr = allr[sm.first_vertex:sm.first_vertex + sm.vertex_count]
            if not rr or not sm.triangles:
                continue
            at = {"POSITION": b.accessor([gltf.to_gl(dec_pos(m, x)) for x in rr], "VEC3", bounds=True)}
            if "normal" in a:
                at["NORMAL"] = b.accessor([gltf.to_gl(norm(dec_vec(x, a["normal"]))) for x in rr], "VEC3")
            for k, key in (("uv0", "TEXCOORD_0"), ("uv1", "TEXCOORD_1")):
                if k in a and m.X:
                    at[key] = b.accessor([dec_uv(m, x, a[k]) for x in rr], "VEC2")
            if "color" in a:
                at["COLOR_0"] = b.accessor([tuple(x[a["color"]:a["color"] + 4]) for x in rr], "VEC4", gltf.UBYTE,
                                           normalized=True)
            if skin is not None:
                js, ws, past = [], [], 0
                for x in rr:
                    sl, w = x[a["bones"]:a["bones"] + 4], x[a["weights"]:a["weights"] + 4]
                    j = []
                    for si, wi in zip(sl, w):  # a few props use slots past their bone map: read as bone indices (G)
                        past += bool(wi) and si >= len(sm.bone_map)
                        j.append(0 if not wi else sm.bone_map[si] if si < len(sm.bone_map) else
                                 si if si < len(m.bones) else 0)
                    js.append(tuple(j))
                    ws.append(tuple(wi / 255 for wi in w))
                if past:
                    print(f"  {r.name} submesh {sm.index}: {past} weights use slots past the bone map "
                          f"({len(sm.bone_map)}); read as bone indices", file=sys.stderr)
                at["JOINTS_0"] = b.accessor(js, "VEC4", gltf.USHORT)
                at["WEIGHTS_0"] = b.accessor(ws, "VEC4")
            idx = [i - sm.first_vertex for t in triangles(m, sm) for i in t]
            label = material_label(sm, m, names)
            if label not in mats:
                md = {"name": label, "pbrMetallicRoughness": {"metallicFactor": 0.0, "roughnessFactor": 0.8}}
                if textures:
                    if sm.material not in images:
                        png = diffuse_png(s, sm.material, archive, cache)
                        images[sm.material] = b.image(*png) if png else None
                    if images[sm.material] is not None:
                        md["pbrMetallicRoughness"]["baseColorTexture"] = {"index": images[sm.material]}
                mats[label] = b.add("materials", md)
            prims.append({"attributes": at, "material": mats[label],
                          "indices": b.accessor(idx, "SCALAR", gltf.UINT if len(rr) > 0xFFFF else gltf.USHORT,
                                                target=gltf.ELEMENT_ARRAY_BUFFER)})
        node = {"name": r.name, "mesh": b.add("meshes", {"name": r.name, "primitives": prims}),
                "extras": {"steep_mesh_id": f"{r.id:#x}", "steep_entry": f"{s.entry:#x}"}}  # Blender keeps these
        if skin is not None:
            node["skin"] = skin
        doc["scenes"][0]["nodes"].append(b.add("nodes", node))
    with open(path, "wb") as f:
        f.write(b.glb())
    return len(meshes), len(rest)


def match_name(label: str | None, meshes: dict) -> str | None:
    label = re.sub(r"\.\d{3}$", "", label or "")
    for name in meshes:
        if label in (name, forge.safe_name(name)):
            return name
    return None


def glb_vertices(g: gltf.Gltf, ni: int, worlds: dict, m: Mesh, bones: dict[int, int], new_bones: list,
                 rigid: int | None, label: str) -> dict[str, tuple[list[Vtx], list[tuple]]]:
    """A glTF mesh node's triangles in Steep's space, per material name. A skinned node's positions are its bind
    pose (where its armature sits as the mesh's bones do: an armature moved as a whole is undone); joints are
    matched to bones by name hash, and joints the mesh lacks are appended to `new_bones` (rest pose from the
    skin). Other nodes are placed by their world transform and bound to `rigid` (a bone index) if given."""
    doc = g.doc
    nodes = doc["nodes"]
    node = nodes[ni]
    skin = doc["skins"][node["skin"]] if "skin" in node else None
    xform = worlds[ni]
    joint_bone = []
    if skin:
        ibms = (g.values(skin["inverseBindMatrices"]) if "inverseBindMatrices" in skin
                else [gltf.IDENTITY] * len(skin["joints"]))
        hashes = [bone_hash(nodes[j].get("name", f"joint{j}")) for j in skin["joints"]]
        steep = {h: gltf.mat_to_gl(gltf.mat_inv_rigid(M)) for h, M in m.bones}
        places = [gltf.mat_mul(gltf.mat_inv(list(ib)), gltf.mat_inv_rigid(steep[h]))
                  for h, ib in zip(hashes, ibms) if h in steep]
        A = places[0] if places else gltf.IDENTITY
        if not places:
            print(f"  {label}: no joint of the armature is a bone of this mesh; positions taken as they are",
                  file=sys.stderr)
        elif max(max(abs(x - y) for x, y in zip(A, P)) for P in places) > 1e-3:
            print(f"  {label}: the armature's bones do not sit where this mesh's bones do (moved in edit mode?); "
                  "positions taken relative to the first matching bone", file=sys.stderr)
        xform = gltf.mat_inv(A)
        for h, ib in zip(hashes, ibms):
            if h not in bones:
                joint_bone.append(None)  # appended to the mesh only if a vertex uses it
            else:
                joint_bone.append(bones[h])
        pending = {k: (h, gltf.mat_from_gl(gltf.mat_mul(list(ib), A))) for k, (h, ib) in enumerate(zip(hashes, ibms))
                   if h not in bones}
    nm = gltf.normal_matrix(xform)
    out: dict[str, tuple[list[Vtx], list[tuple]]] = {}
    for pi, prim in enumerate(doc["meshes"][node["mesh"]]["primitives"]):
        if prim.get("mode", 4) != 4:
            print(f"  {label}: primitive {pi} is not triangles, left out", file=sys.stderr)
            continue
        at = prim["attributes"]
        P = g.values(at["POSITION"])
        N = g.values(at["NORMAL"]) if "NORMAL" in at else None
        T0 = g.values(at["TEXCOORD_0"]) if "TEXCOORD_0" in at else None
        T1 = g.values(at["TEXCOORD_1"]) if "TEXCOORD_1" in at else None
        C0 = g.values(at["COLOR_0"]) if "COLOR_0" in at else None
        sets = []
        k = 0
        while skin and f"JOINTS_{k}" in at and f"WEIGHTS_{k}" in at:
            sets.append((g.values(at[f"JOINTS_{k}"]), g.values(at[f"WEIGHTS_{k}"])))
            k += 1
        verts = []
        for i, p in enumerate(P):
            v = Vtx(gltf.from_gl(gltf.mat_apply(xform, p)))
            if N is not None:
                v.normal = norm(gltf.from_gl(gltf.mat_apply(nm, N[i], 0.0)))
            if T0 is not None:
                v.uv = tuple(T0[i])
            if T1 is not None:
                v.uv1 = tuple(T1[i])
            if C0 is not None:
                c = list(C0[i]) + [1.0] * (4 - len(C0[i]))
                v.color = bytes(max(0, min(255, round(x * 255))) for x in c)
            if sets:
                skin_ = []
                for js, ws in sets:
                    for j, w in zip(js[i], ws[i]):
                        if w <= 0:
                            continue
                        if joint_bone[j] is None:
                            h, M = pending[j]
                            new_bones.append((h, M))
                            bones[h] = len(m.bones) + len(new_bones) - 1
                            joint_bone[j] = bones[h]
                        skin_.append((joint_bone[j], w))
                v.skin = skin_
            elif rigid is not None:
                v.skin = [(rigid, 1.0)]
            verts.append(v)
        idx = g.values(prim["indices"]) if "indices" in prim else list(range(len(P)))
        tris = [(idx[i], idx[i + 1], idx[i + 2]) for i in range(0, len(idx) - 2, 3)]
        mi = prim.get("material")
        mname = doc["materials"][mi].get("name", f"material{mi}") if mi is not None else ""
        vs, ts = out.setdefault(re.sub(r"\.\d{3}$", "", mname), ([], []))
        ts += [tuple(len(vs) + c for c in t) for t in tris]
        vs += verts
    return out


def stored_geometry(m: Mesh, sm: Submesh) -> tuple[list[Vtx], list[tuple]]:
    """A submesh's own vertices and triangles as import input (`--add`): they come out with their bytes kept."""
    a = m.attrs()
    rr = rows(m, m.main[0])[sm.first_vertex:sm.first_vertex + sm.vertex_count]
    verts = []
    for x in rr:
        v = Vtx(dec_pos(m, x), dec_uv(m, x, a["uv0"]), dec_vec(x, a["normal"]), row=x)
        if "weights" in a:
            v.skin = [(sm.bone_map[x[a["bones"] + i]], x[a["weights"] + i]) for i in range(4) if x[a["weights"] + i]]
        verts.append(v)
    return verts, [tuple(i - sm.first_vertex for i in t) for t in triangles(m, sm)]


def submesh_for(m: Mesh, mname: str, names: dict[int, str], only_one: bool) -> Submesh | None:
    mm = re.fullmatch(r"(.*)#sm(\d+)", mname)
    if mm:
        return next((sm for sm in m.submeshes if sm.index == int(mm[2])), None)
    cand = [sm for sm in m.submeshes if (names.get(sm.material) or f"0x{sm.material:x}") == mname]
    if len(cand) == 1:
        return cand[0]
    if only_one and len(m.submeshes) == 1:
        return m.submeshes[0]
    return None


def rigid_bone(g: gltf.Gltf, ni: int, bones: dict[int, int], default: str | None, label: str) -> int | None:
    """The bone an unskinned object is bound to: its name's `@Bone` suffix, else the joint it hangs under
    (Blender's Parent to Bone), else `default` (--bone)."""
    nodes = g.doc["nodes"]
    joints = {j for sk in g.doc.get("skins", []) for j in sk["joints"]}
    parent = {c: i for i, nd in enumerate(nodes) for c in nd.get("children", [])}
    name = nodes[ni].get("name", "")
    want = name.rsplit("@", 1)[1] if "@" in name else None
    p = parent.get(ni)
    while want is None and p is not None:
        if p in joints:
            want = nodes[p].get("name", "")
        p = parent.get(p)
    want = want or default
    if not want:
        return None
    if bone_hash(want) not in bones:
        sys.exit(f"{label}: object {name!r} is bound to {want}, which is not a bone of this mesh "
                 f"({', '.join(bone_name(x) for x in bones)})")
    return bones[bone_hash(want)]


def import_glb(s: Source, res: list, a) -> list[str]:
    """`import --glb`: rebuild the meshes that the files' objects are named after (objects named after none go
    into the `--to` meshes) from them; several objects for one mesh are merged."""
    names = names_by_id(s)
    meshes = {r.name: (k, r) for k, r in mesh_resources(s, None)}
    to = [r.name for _, r in mesh_resources(s, a.to)] if a.to else []
    targets: dict[str, list[tuple]] = {}
    for path in a.glb:
        g = gltf.load(path)
        worlds = g.world_matrices()
        nodes = g.doc.get("nodes", [])
        for i, nd in enumerate(nodes):
            if "mesh" not in nd:
                continue
            name = None if a.ignore_names else match_name(nd.get("name", "").split("@")[0], meshes) or match_name(
                g.doc["meshes"][nd["mesh"]].get("name"), meshes)
            for t in [name] if name else to:
                targets.setdefault(t, []).append((g, i, worlds))
            if not name and not to:
                print(f"  {os.path.basename(path)}: object {nd.get('name')!r} is named after no mesh here, left out "
                      "(see --to)", file=sys.stderr)
    if not targets:
        sys.exit(f"no object is named after a mesh of {s.name} ({', '.join(meshes)}); name it so, or give --to")
    done = []
    for name, objs in targets.items():
        k, r = meshes[name]
        m = parse_mesh(r.payload)
        bones = {h: i for i, (h, _) in enumerate(m.bones)}
        new_bones: list = []
        geo: dict[str, tuple[list, list]] = {}
        for g, ni, worlds in objs:
            rigid = None if "skin" in g.doc["nodes"][ni] else rigid_bone(g, ni, bones, a.bone, name)
            for mname, (vs, ts) in glb_vertices(g, ni, worlds, m, bones, new_bones, rigid, name).items():
                gv, gt = geo.setdefault(mname, ([], []))
                gt += [tuple(len(gv) + c for c in t) for t in ts]
                gv += vs
        items: dict[int, tuple] = {}
        for mname, (vs, ts) in geo.items():
            sm = submesh_for(m, mname, names, len(geo) == 1)
            if sm is None and a.to:  # e.g. a LOD without that submesh
                print(f"  {name}: no submesh with material {mname!r}, left out", file=sys.stderr)
                continue
            if sm is None:
                sys.exit(f"{name}: material {mname!r} is no submesh's of {name}; the submeshes' materials are "
                         + ", ".join(material_label(x, m, names) for x in m.submeshes))
            if sm.index not in items:
                items[sm.index] = (sm, *stored_geometry(m, sm)) if a.add else (sm, [], [])
            iv, it = items[sm.index][1:]
            it += [tuple(len(iv) + c for c in t) for t in ts]
            iv += vs
        payload = add_bones(r.payload, new_bones) if new_bones else r.payload
        m = parse_mesh(payload)
        subs = {sm.index: sm for sm in m.submeshes}
        new, fallback = import_topology(m, [(subs[i], vs, ts) for i, (_, vs, ts) in items.items()])
        m2 = parse_mesh(new)
        for i, (_, vs, ts) in sorted(items.items()):
            s2 = next(x for x in m2.submeshes if x.index == i)
            fb = fallback.get(i, 0)
            print(f"  {name} submesh {i}: {subs[i].vertex_count} -> {s2.vertex_count} vertices, {subs[i].triangles} "
                  f"-> {s2.triangles} triangles" + (f", bone map {len(subs[i].bone_map)} -> {len(s2.bone_map)}"
                                                    if s2.bone_map != subs[i].bone_map else "")
                  + (f"; {fb} vertices without weights took the nearest original vertex's" if fb else ""))
        kept = [x.index for x in m2.submeshes if x.index not in items]
        if kept:
            print(f"  {name}: submesh(es) {kept} not in the file, kept as they are")
        if new_bones:
            print(f"  {name}: bones added: {', '.join(bone_name(h) for h, _ in new_bones)}")
        probs = check_mesh(new)
        if probs:
            raise MeshError(f"{name}: the rebuilt mesh fails its check: {'; '.join(probs)}")
        res[k] = forge.Resource(r.id, r.type, r.name, new, r.flag, r.pre, r.extra)
        done.append(name)
    return done


def cmd_import(a):
    s = load_source(a.src, a.entry)
    res = list(s.resources)
    if bool(a.glb) == bool(a.obj):
        sys.exit("import: give --glb or --obj")
    if a.glb:
        done = import_glb(s, res, a)
        base = save(s, res, a.out, {"mesh_import": {"meshes": done, "glb": [os.path.basename(p) for p in a.glb]}})
        print(f"{base}/: {s.name} with {len(done)} imported mesh(es)")
        return
    meshes = {r.name: (k, r) for k, r in mesh_resources(s, None)}
    plan: dict[str, list] = {}
    for path in a.obj:
        obj = read_obj(path)
        name, sub = match_obj(path, obj, meshes)
        plan.setdefault(name, []).append((sub, obj, path))
    done = []
    for name, items in plan.items():
        k, r = meshes[name]
        m = parse_mesh(r.payload)
        subs = {sm.index: sm for sm in m.submeshes}
        for sub, _, path in items:
            if sub not in subs:
                sys.exit(f"{path}: {name} has no submesh {sub}")
        if a.topology:
            new, _ = import_topology(m, [(subs[sub], *obj_geometry(obj)) for sub, obj, _ in items])
            m2 = parse_mesh(new)
            for sub, _, path in items:
                s2 = next(x for x in m2.submeshes if x.index == sub)
                print(f"  {path} -> {name} submesh {sub}: {subs[sub].vertex_count} -> {s2.vertex_count} vertices, "
                      f"{subs[sub].triangles} -> {s2.triangles} triangles")
            probs = check_mesh(new)
            if probs:
                raise MeshError(f"{name}: the rebuilt mesh fails its check: {'; '.join(probs)}")
        else:
            vb, _, _ = m.main
            rr = rows(m, vb)
            for sub, obj, path in items:
                ch = import_same(m, subs[sub], obj, rr)
                print(f"  {path} -> {name} submesh {sub}: {ch} of {subs[sub].vertex_count} vertices changed")
            new = rewrite(m, rr)
            parse_mesh(new)
        res[k] = forge.Resource(r.id, r.type, r.name, new, r.flag, r.pre, r.extra)
        done.append(name)
    base = save(s, res, a.out, {"mesh_import": {"meshes": done, "obj": [os.path.basename(p) for p in a.obj]}})
    print(f"{base}/: {s.name} with {len(done)} imported mesh(es)")


# --- check ------------------------------------------------------------------------------------------------------

class Ident:
    inflate = 0.0
    split_normals = False
    scale = None
    pivot = None
    translate = None


def check_mesh(p: bytes) -> list[str]:
    """Parse, sanity-check and identity-rewrite one mesh payload; returns problems."""
    probs = []
    m = parse_mesh(p)
    vb, ib, relative = m.main
    if vb.size % m.stride:
        probs.append("vertex buffer is not a whole number of vertices")
    nv = vb.size // m.stride
    for sm in m.submeshes:
        for t in triangles(m, sm):
            if max(t) >= nv:
                probs.append(f"submesh {sm.index}: index past the vertex buffer")
                break
    if apply_positions(m, transform(m, Ident)) != p:
        probs.append("identity rewrite changed the payload")
    r = rows(m, vb)
    if any(enc_pos(m, x, dec_pos(m, x)) != x for x in r):
        probs.append("a position does not re-encode to its own bytes")
    if m.s1 and m.s1["vb"].size and m.vb2.size and sorted(r) != sorted(rows(m, m.vb2)):
        probs.append("section 2 is not a reordering of section 1")
    pts = [dec_pos(m, x) for x in r]
    bb = struct.unpack_from("<8f", p, m.bbox_off)
    if pts:
        lo, hi = bounds(pts)
        tol = 2 * abs(m.S) / Q + 1e-6
        if any(abs(lo[i] - bb[i]) > tol or abs(hi[i] - bb[4 + i]) > tol for i in range(3)):
            probs.append("decoded positions do not span the stored bounds")
    return probs


def cmd_check(a):
    fg = forge.Forge(a.archive)
    seen, bad, n = set(), 0, 0
    for e in fg.entries:
        if e.type not in (MESH_TYPE, 0x51DC6B80) or (a.grep and a.grep.lower() not in e.name.lower()):
            continue
        for r in forge.parse_datafile(fg.read(e)).resources:
            if r.type != MESH_TYPE or r.id in seen:
                continue
            seen.add(r.id)
            n += 1
            try:
                probs = check_mesh(r.payload)
            except Exception as ex:  # report and go on
                probs = [f"{type(ex).__name__}: {ex}"]
            if probs:
                bad += 1
                print(f"{r.id:#x} {r.name}: {'; '.join(probs)}")
        if a.limit and n >= a.limit:
            break
    print(f"{a.archive}: {n} meshes checked, {bad} with problems")
    return 1 if bad else 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)

    def src(p):
        p.add_argument("src", help="unpacked folder, .datafile, .payload, or ARCHIVE.forge")
        p.add_argument("entry", nargs="?", help="with an archive: the entry (id or exact name)")
        p.add_argument("--mesh", action="append", help="only meshes whose name contains this (or id); repeatable")

    p = sp.add_parser("info", help="layout of each mesh: formats, counts, bounds, submeshes, skinning")
    src(p)
    p.set_defaults(fn=cmd_info)
    p = sp.add_parser("export", help="OBJ per mesh (LOD) and submesh")
    src(p)
    p.add_argument("-o", "--out", required=True, help="a folder for OBJs, or a .glb file (armature + weights, "
                   "for Blender)")
    p.add_argument("--skin", action="store_true", help="OBJ: also write <obj>.skin.json (bones, bone map, weights)")
    p.add_argument("--textures", nargs="?", const=True, metavar="ARCHIVE", help=".glb: embed each material's "
                   "diffuse texture (numpy); give an archive to search when the source is not one")
    p.add_argument("--rig", action="append", help=".glb: add the bones of this entry's meshes to the armature (an "
                   "entry of the source archive's stack, or ARCHIVE.forge:ENTRY; repeatable), to weight to bones the "
                   "mesh lacks")
    p.set_defaults(fn=cmd_export)
    p = sp.add_parser("edit", help="move vertices (inflate/scale/translate) and write an unpacked datafile folder")
    src(p)
    p.add_argument("-o", "--out", required=True, help="folder for the result (e.g. mods/<mod>/client/forge/<base>)")
    p.add_argument("--inflate", type=float, default=0.0, help="metres along the vertex normals (welded at seams)")
    p.add_argument("--split-normals", action="store_true", help="inflate along each vertex's own normal")
    p.add_argument("--scale", help="factor or fx,fy,fz about --pivot (default: each mesh's bounds centre)")
    p.add_argument("--pivot", help="x,y,z")
    p.add_argument("--translate", help="x,y,z metres")
    p.set_defaults(fn=cmd_edit)
    p = sp.add_parser("import", help="a .glb (any topology, with weights) or OBJs (as export writes them) replace "
                      "meshes")
    src(p)
    p.add_argument("--glb", action="append", help="a .glb/.gltf (e.g. from Blender; repeatable): objects named after "
                   "meshes, materials after the submeshes' materials; any topology (formats 0, 2, 6, 8)")
    p.add_argument("--to", action="append", help="--glb: objects named after no mesh go into these meshes (name "
                   "contains or id; repeatable), e.g. --to LOD for all LODs")
    p.add_argument("--bone", help="--glb: bind unskinned objects without `@Bone` or a parent bone to this bone")
    p.add_argument("--ignore-names", action="store_true", help="--glb: every object goes into the --to meshes, "
                   "also one named after a mesh (e.g. LOD0's geometry into every LOD)")
    p.add_argument("--add", action="store_true", help="--glb: add the file's geometry to the submeshes instead of "
                   "replacing them (for add-on pieces)")
    p.add_argument("--obj", action="append", help="an OBJ named <mesh>_sm<k>; repeatable")
    p.add_argument("--topology", action="store_true", help="the OBJs may have other vertices/faces (skinning, uv1 "
                   "and the other bytes are taken from the nearest original vertex)")
    p.add_argument("-o", "--out", required=True)
    p.set_defaults(fn=cmd_import)
    p = sp.add_parser("check", help="parse every mesh of an archive, check it and its identity rewrite")
    p.add_argument("archive")
    p.add_argument("-g", "--grep", help="entry name contains")
    p.add_argument("--limit", type=int, default=0, help="stop after this many meshes")
    p.set_defaults(fn=cmd_check)
    a = ap.parse_args(argv)
    try:
        return a.fn(a) or 0
    except (MeshError, gltf.GltfError) as ex:
        sys.exit(f"mesh: {ex}")


if __name__ == "__main__":
    sys.exit(main())
