#!/usr/bin/env python3
"""Master Chief as the default male rider's outfit: converts a skinned glTF onto Steep's skeleton and writes this
mod's .forge entries (client/forge/<Base>/).

The model is "Master Chief High Poly" by HarrisonHag1 (CC-BY-4.0,
https://sketchfab.com/3d-models/master-chief-high-poly-18f5ee1d13904939971601055102c91b), unpacked into
models/master_chief_high_poly/ (not in git). What it does:
1. Retarget: the model's bones (bip_pelvis, bip_upperArm_L, f_index.01.L, ...) are mapped to Steep's (Reference,
   LeftArm, LeftHandIndex1, ...). Each bone is rotated so it points along Steep's bone and moved onto Steep's joint
   (the rest poses of the jacket, gloves, pants and boots); the vertices follow by their own weights. Both stand
   in an A-pose facing -Y, so the changes are small. The weights are renamed to Steep's bones.
2. Textures: the 4 materials (armor, suit, helmet, visor) are packed 2x2 into one 2048 atlas, for the jacket's
   diffuse, normal, specular and mask maps, and the UVs are moved into their quarter.
3. Mesh: the whole body replaces STP_M_Jacket_001 (all 4 LODs, and the LOD0 copy in the Alps archive), with the
   leg and finger bones the jacket lacks added (`mesh.py import --glb`).
4. The rest of the default male outfit (head, hair, hat, helmets, goggles, gloves, pants, boots, scarves,
   backpacks, the jacket's zippers) is shrunk to a point (`mesh.py edit --scale 0.001`), so nothing pokes through.
Needs numpy and Pillow (the textures are JPEG).
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
TOOLS = os.path.normpath(os.path.join(HERE, "..", "..", "tools", "forge"))
sys.path.insert(0, TOOLS)
import forge  # noqa: E402
import gltf  # noqa: E402
import mesh  # noqa: E402
import texture  # noqa: E402

ONB = "DataPC_extra_chr_onboarding"
JACKET = "STP_M_Jacket_001"
TEX = {"diffuse": "STP_M_Jacket_001_T01_DiffuseMap", "normal": "STP_M_Jacket_001_T01_NormalMap",
       "specular": "STP_M_Jacket_001_T01_SpecularMap", "mask": "STP_M_Jacket_001_T01_Mask1Map"}
REST_FROM = ["STP_M_Jacket_001", "STP_M_Glove_001", "STP_M_Pants_001", "STP_M_SkiBoot_001"]  # first one wins
HIDE = ["STP_M_Jacket_001_Zippers01", "STP_M_Jacket_002", "STP_M_Jacket_002_Zippers01", "STP_M_Glove_001",
        "STP_M_Pants_001", "STP_M_Pants_005", "STP_M_Pants_005_Zippers01", "STP_M_SkiBoot_001",
        "STP_M_SkiBoot_001_Cut", "STP_M_SnowboardBoot_001", "STP_M_SnowboardBoot_001_Cut", "STP_M_Head_001",
        "STP_M_Head_001_Beard", "STP_M_Head_001_TeethUp", "STP_M_Head_001_TeethDn", "STP_M_Head_001_Tongue",
        "STP_M_Head_002", "STP_M_Head_002_TeethUp", "STP_M_Head_002_TeethDn", "STP_M_Head_002_Tongue",
        "STP_M_Head_004_Hair", "STP_M_Hat_001", "STP_M_Helmet_001", "STP_M_Helmet_001_Sangle", "STP_M_Helmet_003",
        "STP_M_Helmet_003_Sangle", "STP_M_Helmet_003_BP", "STP_M_Goggle_001", "STP_M_Goggle_002",
        "STP_M_Goggle_001_BP", "STP_M_Scarf_001", "STP_M_Scarf_002", "STP_M_Backpack_001",
        "STP_M_Backpack_001_Zipper01", "STP_M_Backpack_001_Zipper02", "STP_M_BackpackBelt_Default_001",
        "STP_M_BackpackBelt_Large_001", "STP_M_Backpack_NoBJ_001", "STP_M_Harness_001"]

# the model's bones (numeric suffix stripped) -> Steep's; fingers are handled by finger_bone
BONES = {"bip_pelvis": "Reference", "bip_spine_0": "Spine", "bip_spine_1": "Spine1", "bip_spine_2": "Spine2",
         "bip_neck": "Neck", "bip_head": "Head"}
for _c, _s in (("L", "Left"), ("R", "Right")):
    BONES.update({f"bip_collar_{_c}": f"{_s}Shoulder", f"bip_upperArm_{_c}": f"{_s}Arm",
                  f"bip_lowerArm_{_c}": f"{_s}ForeArm", f"bip_hand_{_c}": f"{_s}Hand", f"bip_hip_{_c}": f"{_s}UpLeg",
                  f"bip_knee_{_c}": f"L_{_s}Leg", f"bip_foot_{_c}": f"{_s}Foot"})
# Steep bone -> the bone it points at (for its direction); without one it keeps its parent's rotation
CHILD = {"Reference": "Spine", "Spine": "Spine1", "Spine1": "Spine2", "Spine2": "Neck", "Neck": "Head"}
for _s in ("Left", "Right"):
    CHILD.update({f"{_s}Shoulder": f"{_s}Arm", f"{_s}Arm": f"{_s}ForeArm", f"{_s}ForeArm": f"{_s}Hand",
                  f"{_s}Hand": f"{_s}HandMiddle1", f"{_s}UpLeg": f"L_{_s}Leg", f"L_{_s}Leg": f"{_s}Foot"})
    for _f in ("Thumb", "Index", "Middle", "Ring", "Pinky"):
        CHILD.update({f"{_s}Hand{_f}1": f"{_s}Hand{_f}2", f"{_s}Hand{_f}2": f"{_s}Hand{_f}3"})

ATLAS = {"chiefarmor": (0, 0), "chiefsuit": (1, 0), "chiefhelmet": (0, 1), "chiefvisor": (1, 1)}  # quarters
SIZE, PAD = 2048, 6 / 2048


def run(*cmd):
    p = subprocess.run([sys.executable, *cmd], capture_output=True, text=True)
    if p.returncode:
        sys.exit(f"{' '.join(cmd[:3])}: {p.stderr or p.stdout}")
    return p.stdout


# --- the model ------------------------------------------------------------------------------------------------------

def finger_bone(name: str, side: str) -> str | None:
    m = re.match(r"(?:f_)?(index|middle|ring|pinky|thumb)\.0([123])", name)
    return f"{side}Hand{m[1].capitalize()}{m[2]}" if m else None


def model_bones(g: gltf.Gltf) -> tuple[list, list, dict]:
    """Per joint of the skin: its Steep bone (or None: it follows its parent), its skinning matrix (joint world x
    inverse bind) and the rest position of every mapped joint (glTF space, the model's units)."""
    d = g.doc
    W = g.world_matrices()
    sk = d["skins"][0]
    joints = sk["joints"]
    parent = {c: i for i, nd in enumerate(d["nodes"]) for c in nd.get("children", [])}
    names = [re.sub(r"_\d+$", "", d["nodes"][j].get("name", "")) for j in joints]

    def side(j):
        while j is not None:
            m = re.match(r"bip_hand_([LR])", re.sub(r"_\d+$", "", d["nodes"][j].get("name", "")))
            if m:
                return "Left" if m[1] == "L" else "Right"
            j = parent.get(j)
        return None
    steep, pos = [], {}
    for j, n in zip(joints, names):
        s = BONES.get(n) or (finger_bone(n, side(j)) if side(j) else None)
        if s:
            pos.setdefault(s, np.array(gltf.from_gl(W[j][12:15])))
        steep.append(s)
    index = {j: k for k, j in enumerate(joints)}
    for k, j in enumerate(joints):  # unmapped joints (ends, root) follow the nearest mapped ancestor
        p = j
        while steep[k] is None and p in parent:
            p = parent[p]
            if p in index and steep[index[p]]:
                steep[k] = steep[index[p]]
        steep[k] = steep[k] or "Reference"
    skin = [gltf.mat_mul(W[j], list(m)) for j, m in zip(joints, g.values(sk["inverseBindMatrices"]))]
    return steep, skin, pos


def model_geometry(g: gltf.Gltf, skin: list) -> list[dict]:
    """Each primitive at rest in Steep's axes (still the model's units): positions, normals, UVs, joints, weights."""
    d = g.doc
    out = []
    for nd in d["nodes"]:
        if "mesh" not in nd:
            continue
        for pr in d["meshes"][nd["mesh"]]["primitives"]:
            at = pr["attributes"]
            P = np.array(g.values(at["POSITION"]))
            N = np.array(g.values(at["NORMAL"]))
            J = np.array(g.values(at["JOINTS_0"]), int)
            Wt = np.array(g.values(at["WEIGHTS_0"]))
            S = np.array(skin).reshape(-1, 4, 4).transpose(0, 2, 1)  # row-major 4x4 per joint
            M = np.einsum("vk,vkij->vij", Wt, S[J])
            p = np.einsum("vij,vj->vi", M[:, :3, :3], P) + M[:, :3, 3]
            n = np.einsum("vij,vj->vi", M[:, :3, :3], N)
            idx = np.array(g.values(pr["indices"]), int).reshape(-1, 3)
            out.append({"pos": p[:, [0, 2, 1]] * [1, -1, 1], "nrm": n[:, [0, 2, 1]] * [1, -1, 1],
                        "uv": np.array(g.values(at["TEXCOORD_0"])), "joints": J, "weights": Wt, "tris": idx,
                        "material": d["materials"][pr["material"]]["name"]})
    return out


# --- Steep's rest pose ----------------------------------------------------------------------------------------------

def steep_rest(game: str) -> dict:
    """Bone -> (inverse bind matrix, rest matrix) from the default items' LOD0 (the first item that has it)."""
    rest = {}
    for item in REST_FROM:
        s = mesh.load_source(os.path.join(game, ONB + ".forge"), item)
        r = [r for _, r in mesh.mesh_resources(s, ["LOD0"])][0]
        for h, M in mesh.parse_mesh(r.payload).bones:
            rest.setdefault(mesh.bone_name(h), (list(M), gltf.mat_inv_rigid(M)))
    return rest


def rotation(a, b) -> np.ndarray:
    """The smallest rotation taking direction a onto b."""
    a, b = a / np.linalg.norm(a), b / np.linalg.norm(b)
    v, c = np.cross(a, b), float(np.dot(a, b))
    if np.linalg.norm(v) < 1e-9:
        return np.eye(3) if c > 0 else -np.eye(3)
    k = np.array([[0, -v[2], v[1]], [v[2], 0, -v[0]], [-v[1], v[0], 0]])
    return np.eye(3) + k + k @ k * ((1 - c) / np.linalg.norm(v) ** 2)


def fit(model_pos: dict, rest: dict, sole: float) -> tuple[float, dict]:
    """Per Steep bone a transform (rotation, scale, move) taking the model's bone onto Steep's. The feet are also
    squashed in height so the soles (the model's lowest point, `sole`) end on the floor, as Steep's do."""
    sp = {b: np.array(rest[b][1][12:15]) for b in model_pos if b in rest}
    missing = [b for b in model_pos if b not in rest]
    if missing:
        sys.exit(f"Steep rest pose missing for {missing}")
    scale = (sp["Head"][2] - sp["LeftFoot"][2]) / (model_pos["Head"][2] - model_pos["LeftFoot"][2])
    xf, rot = {}, {}
    order = sorted(model_pos, key=lambda b: (b != "Reference", b))  # Reference first, then parents before children

    def rot_of(b):
        if b in rot:
            return rot[b]
        c = CHILD.get(b)
        if c in model_pos and c in sp:
            rot[b] = rotation(model_pos[c] - model_pos[b], sp[c] - sp[b])
        else:  # a leaf (head, finger tips, feet): its parent's rotation
            par = next((p for p, ch in CHILD.items() if ch == b), "Reference")
            rot[b] = rot_of(par) if par != b else np.eye(3)
        return rot[b]
    for b in order:
        A = rot_of(b) * scale
        if b.endswith("Foot"):
            A = np.diag([1.0, 1.0, sp[b][2] / ((model_pos[b][2] - sole) * scale)]) @ A
        xf[b] = (A, sp[b] - A @ model_pos[b])
    return scale, xf


# --- textures -------------------------------------------------------------------------------------------------------

def atlas(model: str, out: str) -> dict:
    """The 4 maps of the jacket's texture set from the model's materials, packed 2x2 (upright, as texture.py
    takes them). Returns {map: png path}."""
    tex = os.path.join(model, "textures")
    half = SIZE // 2
    img = {k: np.zeros((SIZE, SIZE, 4), np.int32) for k in ("diffuse", "normal", "specular", "mask")}

    def load(name, fallback):
        p = os.path.join(tex, name)
        if not os.path.exists(p):
            return np.broadcast_to(np.array(fallback, np.int32), (half, half, 4)).copy()
        return texture.resize(texture.read_image(p), half, half)
    for mat, (qx, qy) in ATLAS.items():
        ys, xs = slice(qy * half, (qy + 1) * half), slice(qx * half, (qx + 1) * half)
        img["diffuse"][ys, xs] = load(f"{mat}_baseColor.jpeg", (128, 128, 128, 255))
        nm = load(f"{mat}_normal.png", (128, 128, 255, 255))
        # Steep: X in R, Y in G (pointing up the image, as glTF's), B = 0, A = the jacket's average of its 4th map
        img["normal"][ys, xs] = np.stack([nm[..., 0], nm[..., 1], np.zeros_like(nm[..., 0]),
                                          np.full_like(nm[..., 0], 98)], -1)
        mr = load(f"{mat}_metallicRoughness.png", (0, 128, 0, 255))  # glTF: G roughness, B metalness
        spec = np.clip(30 + 0.55 * mr[..., 2] + 0.25 * (255 - mr[..., 1]), 0, 255)
        img["specular"][ys, xs] = np.stack([spec, spec, spec, np.full_like(spec, 255)], -1)
        cloth = 255 if mat == "chiefsuit" else 0  # Mask1 R: white on the jacket's fabric, black on its hard parts
        img["mask"][ys, xs] = (cloth, 0, 255, 255)
    paths = {}
    for k, v in img.items():
        paths[k] = os.path.join(out, f"chief_{k}.png")
        texture.write_png(paths[k], v)
    return paths


# --- output ---------------------------------------------------------------------------------------------------------

def write_glb(parts: list, xf: dict, rest: dict, path: str) -> tuple[int, int]:
    """The fitted body as one object with the jacket's material, skinned to Steep's bones."""
    bones = sorted({b for p in parts for b in p["bones"]})
    bi = {b: i for i, b in enumerate(bones)}
    pos, nrm, uv, js, ws, idx = [], [], [], [], [], []
    nmat = {k: np.linalg.inv(A).T for k, (A, _) in xf.items()}  # normals: the inverse transpose
    for p in parts:
        base = len(pos)
        qx, qy = ATLAS[p["material"]]
        for k in range(len(p["pos"])):
            v, n = p["pos"][k], p["nrm"][k]
            infl = {}
            for j, w in zip(p["joints"][k], p["weights"][k]):
                if w > 0:
                    infl[p["bones"][j]] = infl.get(p["bones"][j], 0.0) + float(w)
            top = sorted(infl.items(), key=lambda kv: -kv[1])[:4]
            tot = sum(w for _, w in top)
            vp = sum(w * (xf[b][0] @ v + xf[b][1]) for b, w in top) / tot
            vn = sum(w * (nmat[b] @ n) for b, w in top)
            vn = vn / (np.linalg.norm(vn) or 1)
            pos.append(gltf.to_gl(vp.tolist()))
            nrm.append(gltf.to_gl(vn.tolist()))
            u, w_ = (min(max(float(c), 0.0), 1.0) for c in p["uv"][k])  # the model's UVs stay within 0..1
            uv.append(((qx + PAD * 2 + u * (1 - 4 * PAD)) / 2, (qy + PAD * 2 + w_ * (1 - 4 * PAD)) / 2))
            js.append(tuple(bi[b] for b, _ in top) + (0,) * (4 - len(top)))
            ws.append(tuple(w / tot for _, w in top) + (0.0,) * (4 - len(top)))
        idx += [base + int(i) for i in p["tris"].reshape(-1)]
    b = gltf.Builder("powder mods/master_chief/make_chief.py")
    root = b.add("nodes", {"name": "Steep_Rig"})
    jn = [b.add("nodes", {"name": name, **gltf.trs(gltf.mat_to_gl(rest[name][1]))}) for name in bones]
    b.doc["nodes"][root]["children"] = jn
    ibm = b.accessor([gltf.mat_to_gl(rest[name][0]) for name in bones], "MAT4", target=None)
    skin = b.add("skins", {"joints": jn, "inverseBindMatrices": ibm, "skeleton": root})
    mat = b.add("materials", {"name": "STP_M_Jacket_001_ALL"})
    prim = {"attributes": {"POSITION": b.accessor(pos, "VEC3", bounds=True), "NORMAL": b.accessor(nrm, "VEC3"),
                           "TEXCOORD_0": b.accessor(uv, "VEC2"), "JOINTS_0": b.accessor(js, "VEC4", gltf.USHORT),
                           "WEIGHTS_0": b.accessor(ws, "VEC4")},
            "indices": b.accessor(idx, "SCALAR", gltf.UINT, target=gltf.ELEMENT_ARRAY_BUFFER), "material": mat}
    node = b.add("nodes", {"name": "master_chief", "mesh": b.add("meshes", {"name": "master_chief",
                                                                           "primitives": [prim]}), "skin": skin})
    b.doc["scenes"][0]["nodes"] += [root, node]
    with open(path, "wb") as f:
        f.write(b.glb())
    return len(pos), len(idx) // 3


def holders(game: str, ids: list[int]) -> set[tuple[str, str]]:
    """(base archive, entry id) of every datafile that holds one of these resources."""
    out = run(os.path.join(TOOLS, "forge.py"), "where", game, *[x for i in ids for x in ("--id", f"{i:#x}")])
    return {(m[2], m[1]) for m in re.finditer(r"entry (0x[0-9a-f]+) .*\(base (\S+)\)", out)}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--game", required=True, help="the Steep folder")
    ap.add_argument("--model", default=os.path.join(HERE, "..", "..", "models", "master_chief_high_poly"))
    ap.add_argument("-o", "--out", default=os.path.join(HERE, "client", "forge"))
    ap.add_argument("--glb", help="also keep the fitted model (for a look in Blender)")
    a = ap.parse_args()
    g = gltf.load(os.path.join(a.model, "scene.gltf"))
    steep, skin, mpos = model_bones(g)
    rest = steep_rest(a.game)
    parts = model_geometry(g, skin)
    scale, xf = fit(mpos, rest, min(float(p["pos"][:, 2].min()) for p in parts))
    for p in parts:
        p["bones"] = steep
    tmp = tempfile.mkdtemp(prefix="powder_chief_")
    try:
        glb = os.path.join(tmp, "chief.glb")
        nv, nt = write_glb(parts, xf, rest, glb)
        print(f"fitted: {nv} vertices, {nt} triangles, scale {scale:.6f} m per model unit, "
              f"{len(set(steep) - {None})} Steep bones")
        if a.glb:
            shutil.copy(glb, a.glb)
        shutil.rmtree(a.out, ignore_errors=True)
        # the body: every datafile that holds a jacket LOD
        onb = forge.Forge(os.path.join(a.game, ONB + ".forge"))
        jack = mesh.load_source(onb.path, JACKET)
        lod_ids = [r.id for _, r in mesh.mesh_resources(jack, None)]
        for base, entry in sorted(holders(a.game, lod_ids)):
            print(run(os.path.join(TOOLS, "mesh.py"), "import", os.path.join(a.game, base + ".forge"), entry,
                      "--glb", glb, "--to", "LOD", "--ignore-names", "-o", os.path.join(a.out, base)).strip())
        # the textures: every copy of the jacket's 4 maps (and their Mip entries)
        pngs = atlas(a.model, tmp)
        tex_ids = {k: onb.find(name, exact=True)[0].id for k, name in TEX.items()}
        where = holders(a.game, list(tex_ids.values()))
        for k, tid in tex_ids.items():
            for base in sorted({b for b, e in where if int(e, 16) == tid}):
                print(run(os.path.join(TOOLS, "texture.py"), "replace", os.path.join(a.game, base + ".forge"),
                          f"{tid:#x}", "--image", pngs[k], "-o", os.path.join(a.out, base)).strip().splitlines()[-1])
        # the rest of the outfit, shrunk to a point
        hide_ids = []
        for name in HIDE:
            for e in onb.find(name, exact=True):
                df = forge.parse_datafile(onb.read(e))
                hide_ids += [r.id for r in df.resources if r.type == mesh.MESH_TYPE and r.id not in lod_ids]
        hidden = holders(a.game, hide_ids) - holders(a.game, lod_ids)
        for base, entry in sorted(hidden):
            run(os.path.join(TOOLS, "mesh.py"), "edit", os.path.join(a.game, base + ".forge"), entry,
                "--scale", "0.001", "-o", os.path.join(a.out, base))
        print(f"hidden: {len(hidden)} datafiles of the default outfit")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
