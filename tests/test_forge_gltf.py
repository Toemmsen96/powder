"""tools/forge: the glTF writer/reader and mesh.py's bone names and skin quantisation (no game data needed)."""

import math
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "tools", "forge"))
import gltf  # noqa: E402
import mesh  # noqa: E402


def close(a, b, eps=1e-6):
    return all(abs(x - y) <= eps for x, y in zip(a, b))


def rot_z(deg, t=(0.0, 0.0, 0.0)):
    c, s = math.cos(math.radians(deg)), math.sin(math.radians(deg))
    return [c, s, 0, 0, -s, c, 0, 0, 0, 0, 1, 0, *t, 1]


def test_glb_round_trip(tmp_path):
    b = gltf.Builder("test")
    pos = [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (0.0, 1.0, 0.5)]
    at = {"POSITION": b.accessor(pos, "VEC3", bounds=True),
          "JOINTS_0": b.accessor([(1, 2, 0, 0)] * 3, "VEC4", gltf.USHORT),
          "COLOR_0": b.accessor([(255, 0, 128, 255)] * 3, "VEC4", gltf.UBYTE, normalized=True)}
    idx = b.accessor([0, 1, 2], "SCALAR", gltf.USHORT, target=gltf.ELEMENT_ARRAY_BUFFER)
    m = b.add("meshes", {"name": "tri", "primitives": [{"attributes": at, "indices": idx}]})
    n = b.add("nodes", {"name": "tri", "mesh": m, **gltf.trs(rot_z(90, (1, 2, 3)))})
    b.doc["scenes"][0]["nodes"].append(n)
    path = tmp_path / "t.glb"
    path.write_bytes(b.glb())
    g = gltf.load(str(path))
    assert g.values(at["POSITION"]) == pos
    assert g.values(at["JOINTS_0"]) == [(1, 2, 0, 0)] * 3
    assert close(g.values(at["COLOR_0"])[0], (1.0, 0.0, 128 / 255, 1.0))
    assert g.values(idx) == [0, 1, 2]
    assert g.doc["accessors"][at["POSITION"]]["max"] == [1.0, 1.0, 0.5]
    w = g.world_matrices()[n]
    assert close(gltf.mat_apply(w, (1, 0, 0)), (1, 3, 3))  # rotated 90 degrees about Z, then moved


def test_basis_change_keeps_transforms():
    m = gltf.mat_mul(rot_z(30, (0.1, -0.2, 1.5)), [1, 0, 0, 0, 0, 0, 1, 0, 0, -1, 0, 0, 0, 0, 0, 1])
    p = (0.3, -0.7, 1.1)
    assert close(gltf.from_gl(gltf.to_gl(p)), p)
    assert close(gltf.mat_from_gl(gltf.mat_to_gl(m)), m)
    # a point moved in Steep's space and then converted lands where the converted matrix moves it
    assert close(gltf.to_gl(gltf.mat_apply(m, p)), gltf.mat_apply(gltf.mat_to_gl(m), gltf.to_gl(p)))
    assert gltf.to_gl((0, 0, 1)) == (0, 1, 0)  # Steep's up is glTF's up


def test_matrices():
    m = rot_z(-75, (2, 0, -1))
    assert close(gltf.node_matrix(gltf.trs(m)), m)
    assert close(gltf.mat_mul(m, gltf.mat_inv_rigid(m)), gltf.IDENTITY)
    s = [2, 0, 0, 0, 0, 3, 0, 0, 0, 0, 4, 0, 1, 1, 1, 1]
    assert close(gltf.mat_mul(s, gltf.mat_inv(s)), gltf.IDENTITY)
    nm = gltf.normal_matrix(s)  # a normal of the plane x = y keeps perpendicular to it after scaling
    n = gltf.mat_apply(nm, (1, -1, 0), 0.0)
    assert abs(sum(a * b for a, b in zip(n, gltf.mat_apply(s, (1, 1, 0), 0.0)))) < 1e-9


def test_bone_names():
    assert mesh.bone_name(0x07C159A2) == "Head"  # CRC-32 of the name, as in the shipped meshes
    assert mesh.bone_hash("Head") == 0x07C159A2
    assert mesh.bone_hash("Head.001") == 0x07C159A2  # Blender's duplicate suffix
    assert mesh.bone_name(0x1234ABCD) == "0x1234abcd"
    assert mesh.bone_hash("0x1234abcd") == 0x1234ABCD
    assert all(mesh.bone_name(mesh.bone_hash(n)) == n for n in mesh.BONE_NAMES)


def test_quantise_skin():
    bm = [7, 3]
    slots, weights = mesh.quantise_skin([(3, 0.2), (9, 0.5), (7, 0.1), (5, 0.1), (4, 0.05), (3, 0.05)], bm)
    assert sum(weights) == 255 and len(slots) == 4
    assert weights == bytes(sorted(weights, reverse=True))
    assert [bm[s] for s in slots] == [9, 3, 7, 5]  # heaviest first; 9 and 5 appended to the bone map
    assert bm == [7, 3, 9, 5]
    assert mesh.quantise_skin([(1, 1.0)], []) == (bytes([0, 0, 0, 0]), bytes([255, 0, 0, 0]))
    assert mesh.quantise_skin([(1, 0.0)], []) is None
    with pytest.raises(mesh.MeshError):
        mesh.quantise_skin([(200, 1.0)], list(range(128)))
