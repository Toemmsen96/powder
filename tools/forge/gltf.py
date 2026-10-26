"""glTF 2.0 (.glb / .gltf): the subset mesh.py writes and reads, so meshes travel to and from Blender.

Writing: `Builder` collects binary data and accessors and serialises one .glb (JSON chunk + BIN chunk).
Reading: `load` opens a .glb, or a .gltf with an embedded (data:) or external buffer, and `values` decodes an
accessor (any component type, normalised, strided, sparse) into a list of tuples. Python 3.10+, stdlib only.

Steep is Z up, glTF is Y up (Blender converts back on import, so a model sits in Blender as it does in Steep):
`to_gl` / `from_gl` convert points and directions, `mat_to_gl` / `mat_from_gl` 4x4 matrices (column-major, as
both formats store them).
"""

from __future__ import annotations

import base64
import json
import math
import os
import struct

GLB_MAGIC = 0x46546C67
CHUNK_JSON = 0x4E4F534A
CHUNK_BIN = 0x004E4942
# componentType -> (struct code, size, normalised divisor)
COMPONENTS = {5120: ("b", 1, 127.0), 5121: ("B", 1, 255.0), 5122: ("h", 2, 32767.0), 5123: ("H", 2, 65535.0),
              5125: ("I", 4, None), 5126: ("f", 4, None)}
WIDTH = {"SCALAR": 1, "VEC2": 2, "VEC3": 3, "VEC4": 4, "MAT2": 4, "MAT3": 9, "MAT4": 16}
FLOAT, UBYTE, USHORT, UINT = 5126, 5121, 5123, 5125
ARRAY_BUFFER, ELEMENT_ARRAY_BUFFER = 34962, 34963


class GltfError(ValueError):
    pass


def to_gl(v):
    """Steep (Z up) -> glTF (Y up)."""
    return (v[0], v[2], -v[1])


def from_gl(v):
    return (v[0], -v[2], v[1])


def _basis(m: list, to: bool) -> list:
    """C M C^T for column-major 4x4 `m`, C the Steep -> glTF basis change (or its inverse)."""
    rows = [[m[c * 4 + r] for c in range(4)] for r in range(4)]
    perm = [0, 2, 1, 3] if to else [0, 2, 1, 3]
    sign = [1, 1, -1, 1] if to else [1, -1, 1, 1]  # glTF y = Steep z, glTF z = -Steep y (and back)
    out = [[sign[r] * sign[c] * rows[perm[r]][perm[c]] for c in range(4)] for r in range(4)]
    return [out[r][c] for c in range(4) for r in range(4)]


def mat_to_gl(m):
    return _basis(list(m), True)


def mat_from_gl(m):
    return _basis(list(m), False)


IDENTITY = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]


def mat_inv(m):
    """Inverse of any invertible column-major 4x4 (Gauss-Jordan)."""
    a = [[m[c * 4 + r] for c in range(4)] + [1.0 if r == k else 0.0 for k in range(4)] for r in range(4)]
    for c in range(4):
        p = max(range(c, 4), key=lambda r: abs(a[r][c]))
        if abs(a[p][c]) < 1e-12:
            raise GltfError("a transform is not invertible")
        a[c], a[p] = a[p], a[c]
        d = a[c][c]
        a[c] = [x / d for x in a[c]]
        for r in range(4):
            if r != c and a[r][c]:
                f = a[r][c]
                a[r] = [x - f * y for x, y in zip(a[r], a[c])]
    return [a[r][4 + c] for c in range(4) for r in range(4)]


def normal_matrix(m):
    """The inverse transpose of `m`'s 3x3 part (no translation), for directions under a transform with scale."""
    inv = mat_inv([*m[:12], 0.0, 0.0, 0.0, 1.0])
    return [inv[0], inv[4], inv[8], 0.0, inv[1], inv[5], inv[9], 0.0, inv[2], inv[6], inv[10], 0.0,
            0.0, 0.0, 0.0, 1.0]


def mat_mul(a, b):
    """Column-major 4x4 product a * b."""
    return [sum(a[k * 4 + r] * b[c * 4 + k] for k in range(4)) for c in range(4) for r in range(4)]


def mat_inv_rigid(m):
    """Inverse of a rotation + translation (column-major); the bones' matrices are orthonormal."""
    r = [[m[c * 4 + rr] for c in range(3)] for rr in range(3)]
    t = [m[12], m[13], m[14]]
    rt = [[r[c][rr] for c in range(3)] for rr in range(3)]
    ti = [-sum(rt[rr][k] * t[k] for k in range(3)) for rr in range(3)]
    return [rt[0][0], rt[1][0], rt[2][0], 0.0, rt[0][1], rt[1][1], rt[2][1], 0.0,
            rt[0][2], rt[1][2], rt[2][2], 0.0, ti[0], ti[1], ti[2], 1.0]


def mat_apply(m, p, w=1.0):
    return tuple(m[r] * p[0] + m[4 + r] * p[1] + m[8 + r] * p[2] + m[12 + r] * w for r in range(3))


def node_matrix(node: dict) -> list:
    if "matrix" in node:
        return list(node["matrix"])
    tx, ty, tz = node.get("translation", (0, 0, 0))
    x, y, z, w = node.get("rotation", (0, 0, 0, 1))
    sx, sy, sz = node.get("scale", (1, 1, 1))
    r = [1 - 2 * (y * y + z * z), 2 * (x * y + z * w), 2 * (x * z - y * w),
         2 * (x * y - z * w), 1 - 2 * (x * x + z * z), 2 * (y * z + x * w),
         2 * (x * z + y * w), 2 * (y * z - x * w), 1 - 2 * (x * x + y * y)]
    return [r[0] * sx, r[1] * sx, r[2] * sx, 0.0, r[3] * sy, r[4] * sy, r[5] * sy, 0.0,
            r[6] * sz, r[7] * sz, r[8] * sz, 0.0, tx, ty, tz, 1.0]


def trs(m) -> dict:
    """translation/rotation for a rigid column-major matrix (glTF wants decomposable node transforms)."""
    r = [[m[c * 4 + rr] for c in range(3)] for rr in range(3)]
    tr = r[0][0] + r[1][1] + r[2][2]
    if tr > 0:
        s = math.sqrt(tr + 1) * 2
        q = ((r[2][1] - r[1][2]) / s, (r[0][2] - r[2][0]) / s, (r[1][0] - r[0][1]) / s, s / 4)
    elif r[0][0] > r[1][1] and r[0][0] > r[2][2]:
        s = math.sqrt(1 + r[0][0] - r[1][1] - r[2][2]) * 2
        q = (s / 4, (r[0][1] + r[1][0]) / s, (r[0][2] + r[2][0]) / s, (r[2][1] - r[1][2]) / s)
    elif r[1][1] > r[2][2]:
        s = math.sqrt(1 + r[1][1] - r[0][0] - r[2][2]) * 2
        q = ((r[0][1] + r[1][0]) / s, s / 4, (r[1][2] + r[2][1]) / s, (r[0][2] - r[2][0]) / s)
    else:
        s = math.sqrt(1 + r[2][2] - r[0][0] - r[1][1]) * 2
        q = ((r[0][2] + r[2][0]) / s, (r[1][2] + r[2][1]) / s, s / 4, (r[1][0] - r[0][1]) / s)
    n = math.sqrt(sum(c * c for c in q))
    return {"translation": [m[12], m[13], m[14]], "rotation": [c / n for c in q]}


# --- writing ------------------------------------------------------------------------------------------------------

class Builder:
    def __init__(self, generator: str):
        self.bin = bytearray()
        self.doc = {"asset": {"version": "2.0", "generator": generator}, "scene": 0, "scenes": [{"nodes": []}],
                    "nodes": [], "meshes": [], "materials": [], "skins": [], "accessors": [], "bufferViews": [],
                    "buffers": [], "images": [], "textures": [], "samplers": []}

    def view(self, data: bytes, target: int | None = None) -> int:
        while len(self.bin) % 4:
            self.bin.append(0)
        v = {"buffer": 0, "byteOffset": len(self.bin), "byteLength": len(data)}
        if target:
            v["target"] = target
        self.bin += data
        self.doc["bufferViews"].append(v)
        return len(self.doc["bufferViews"]) - 1

    def accessor(self, values: list, kind: str, comp: int = FLOAT, normalized: bool = False,
                 target: int | None = ARRAY_BUFFER, bounds: bool = False) -> int:
        code, size, _ = COMPONENTS[comp]
        w = WIDTH[kind]
        flat = [c for v in values for c in (v if w > 1 else (v,))]
        data = struct.pack(f"<{len(flat)}{code}", *flat)
        while len(data) % 4 and kind != "SCALAR":  # attribute elements are 4-byte aligned
            data += b"\0"
        stride = w * size
        if kind != "SCALAR" and stride % 4:  # e.g. u8 x 2: pad each element
            raise GltfError(f"{kind} of {size}-byte components needs padding")
        a = {"bufferView": self.view(data, target), "componentType": comp, "count": len(values), "type": kind}
        if normalized:
            a["normalized"] = True
        if bounds and values:
            a["min"] = [min(v[i] for v in values) for i in range(w)]
            a["max"] = [max(v[i] for v in values) for i in range(w)]
        self.doc["accessors"].append(a)
        return len(self.doc["accessors"]) - 1

    def add(self, kind: str, obj: dict) -> int:
        self.doc[kind].append(obj)
        return len(self.doc[kind]) - 1

    def image(self, png: bytes, name: str) -> int:
        """A texture (PNG) for a material's baseColorTexture."""
        if not self.doc["samplers"]:
            self.add("samplers", {})
        img = self.add("images", {"name": name, "mimeType": "image/png", "bufferView": self.view(png)})
        return self.add("textures", {"source": img, "sampler": 0, "name": name})

    def glb(self) -> bytes:
        doc = {k: v for k, v in self.doc.items() if v != []}
        while len(self.bin) % 4:
            self.bin.append(0)
        doc["buffers"] = [{"byteLength": len(self.bin)}]
        js = json.dumps(doc, separators=(",", ":")).encode()
        js += b" " * (-len(js) % 4)
        return (struct.pack("<3I", GLB_MAGIC, 2, 12 + 8 + len(js) + 8 + len(self.bin))
                + struct.pack("<2I", len(js), CHUNK_JSON) + js + struct.pack("<2I", len(self.bin), CHUNK_BIN)
                + bytes(self.bin))


# --- reading ------------------------------------------------------------------------------------------------------

class Gltf:
    def __init__(self, doc: dict, buffers: list[bytes]):
        self.doc, self.buffers = doc, buffers

    def view_bytes(self, i: int) -> tuple[bytes, int, int | None]:
        v = self.doc["bufferViews"][i]
        b = self.buffers[v["buffer"]]
        o = v.get("byteOffset", 0)
        return b, o, v.get("byteStride")

    def values(self, i: int) -> list:
        """Accessor `i` as a list of tuples (or of numbers for SCALAR); normalised integers become floats."""
        a = self.doc["accessors"][i]
        code, size, div = COMPONENTS[a["componentType"]]
        w = WIDTH[a["type"]]
        n = a["count"]
        if "bufferView" in a:
            b, o, stride = self.view_bytes(a["bufferView"])
            o += a.get("byteOffset", 0)
            stride = stride or w * size
            fmt = f"<{w}{code}"
            out = [struct.unpack_from(fmt, b, o + k * stride) for k in range(n)]
        else:
            out = [(0,) * w for _ in range(n)]
        sp = a.get("sparse")
        if sp:
            ib, io, _ = self.view_bytes(sp["indices"]["bufferView"])
            icode = COMPONENTS[sp["indices"]["componentType"]][0]
            idx = struct.unpack_from(f"<{sp['count']}{icode}", ib, io + sp["indices"].get("byteOffset", 0))
            vb, vo, _ = self.view_bytes(sp["values"]["bufferView"])
            vo += sp["values"].get("byteOffset", 0)
            for k, j in enumerate(idx):
                out[j] = struct.unpack_from(f"<{w}{code}", vb, vo + k * w * size)
        if a.get("normalized") and div:
            out = [tuple(max(c / div, -1.0) for c in v) for v in out]
        return [v[0] for v in out] if w == 1 else out

    def world_matrices(self) -> dict[int, list]:
        """Each node's world matrix (column-major), over the default scene's tree."""
        nodes = self.doc.get("nodes", [])
        out: dict[int, list] = {}

        def walk(i: int, parent: list):
            m = mat_mul(parent, node_matrix(nodes[i]))
            out[i] = m
            for c in nodes[i].get("children", []):
                walk(c, m)
        ident = [1.0, 0, 0, 0, 0, 1.0, 0, 0, 0, 0, 1.0, 0, 0, 0, 0, 1.0]
        children = {c for nd in nodes for c in nd.get("children", [])}
        for i in range(len(nodes)):
            if i not in children:
                walk(i, ident)
        return out


def load(path: str) -> Gltf:
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] == b"glTF":
        magic, ver, length = struct.unpack_from("<3I", data)
        if ver != 2:
            raise GltfError(f"{path}: glTF version {ver}, not 2")
        o, doc, binchunk = 12, None, None
        while o < min(length, len(data)):
            n, kind = struct.unpack_from("<2I", data, o)
            chunk = data[o + 8:o + 8 + n]
            if kind == CHUNK_JSON:
                doc = json.loads(chunk)
            elif kind == CHUNK_BIN and binchunk is None:
                binchunk = chunk
            o += 8 + n
        if doc is None:
            raise GltfError(f"{path}: no JSON chunk")
    else:
        doc, binchunk = json.loads(data), None
    need = [x for x in doc.get("extensionsRequired", []) if x != "KHR_materials_emissive_strength"]
    if need:  # e.g. Draco or meshopt compression: the data is not in plain accessors
        raise GltfError(f"{path}: needs {', '.join(need)}; export it without compression")
    bufs = []
    for k, b in enumerate(doc.get("buffers", [])):
        uri = b.get("uri")
        if uri is None:
            if binchunk is None:
                raise GltfError(f"{path}: buffer {k} has no data")
            bufs.append(binchunk)
        elif uri.startswith("data:"):
            bufs.append(base64.b64decode(uri.split(",", 1)[1]))
        else:
            with open(os.path.join(os.path.dirname(path), uri), "rb") as f:
                bufs.append(f.read())
    return Gltf(doc, bufs)
