#!/usr/bin/env python3
"""Writes pads.glb: two shoulder pads for STP_M_Jacket_001, as a Blender user would model them.

Each pad is a flattened ellipsoid sitting on a shoulder of the default male rider (Steep's space: metres, Z up,
the rider faces +Y), tilted with the A-pose arm. The objects are named `pad_L@L_LeftArm_NoRoll` and
`pad_R@L_RightArm_NoRoll`, so `mesh.py import --glb` binds each rigidly to that upper-arm bone, and their material
is named after the jacket's (STP_M_Jacket_001_ALL), so they join its submesh. Every UV points at one texel of an
orange patch of the jacket's diffuse map, so the pads come out plain orange. The file holds only this geometry, no
game data. Open it in Blender next to `mesh.py export ... -o jacket.glb` to see how it lines up.
"""

import math
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../../tools/forge"))
import gltf  # noqa: E402

CENTRE = (0.215, 0.02, 1.505)   # above the left shoulder joint, just under the jacket's surface
RADII = (0.085, 0.095, 0.05)    # along the arm, front to back, thickness
TILT = math.radians(-29)        # the shoulder's slope down the arm (about Y)
UV = (0.803, 0.104)             # the orange patch at texel (411, 53) of the 512x512 diffuse (v pointing down)
RINGS, SEGMENTS = 10, 20


def pad(side: float) -> tuple[list, list, list]:
    pos, nrm, tris = [], [], []
    c, s = math.cos(TILT * side), math.sin(TILT * side)
    for i in range(RINGS + 1):
        th = math.pi * i / RINGS
        for j in range(SEGMENTS):
            ph = 2 * math.pi * j / SEGMENTS
            u = (math.sin(th) * math.cos(ph), math.sin(th) * math.sin(ph), math.cos(th))
            p = [u[k] * RADII[k] for k in range(3)]
            n = [u[k] / RADII[k] for k in range(3)]
            p = (p[0] * c + p[2] * s, p[1], -p[0] * s + p[2] * c)    # tilt, then mirror for the right side
            n = (n[0] * c + n[2] * s, n[1], -n[0] * s + n[2] * c)
            pos.append((side * (CENTRE[0] + p[0]), CENTRE[1] + p[1], CENTRE[2] + p[2]))
            ln = math.sqrt(sum(x * x for x in n))
            nrm.append((side * n[0] / ln, n[1] / ln, n[2] / ln))
    for i in range(RINGS):
        for j in range(SEGMENTS):
            a, b = i * SEGMENTS + j, i * SEGMENTS + (j + 1) % SEGMENTS
            q = [a, b, b + SEGMENTS, a + SEGMENTS]
            for t in ((q[0], q[2], q[1]), (q[0], q[3], q[2])):  # counter-clockwise seen from outside
                tris.append(t if side > 0 else (t[0], t[2], t[1]))
    return pos, nrm, tris


def main():
    b = gltf.Builder("powder mods/shoulder_pads/model/make_pads.py")
    mat = b.add("materials", {"name": "STP_M_Jacket_001_ALL"})
    for side, name in ((1, "pad_L@L_LeftArm_NoRoll"), (-1, "pad_R@L_RightArm_NoRoll")):
        pos, nrm, tris = pad(side)
        prim = {"attributes": {"POSITION": b.accessor([gltf.to_gl(p) for p in pos], "VEC3", bounds=True),
                               "NORMAL": b.accessor([gltf.to_gl(n) for n in nrm], "VEC3"),
                               "TEXCOORD_0": b.accessor([UV] * len(pos), "VEC2")},
                "indices": b.accessor([i for t in tris for i in t], "SCALAR", gltf.USHORT,
                                      target=gltf.ELEMENT_ARRAY_BUFFER),
                "material": mat}
        node = b.add("nodes", {"name": name, "mesh": b.add("meshes", {"name": name, "primitives": [prim]})})
        b.doc["scenes"][0]["nodes"].append(node)
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pads.glb")
    with open(out, "wb") as f:
        f.write(b.glb())
    print(out)


if __name__ == "__main__":
    main()
