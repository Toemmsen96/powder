#!/usr/bin/env python3
"""Master Chief as a costume (male and female; --target, default the penguin costume, which is in the store):
converts a skinned glTF onto Steep's skeleton and writes this mod's .forge entries (client/forge/<Base>/).

The model is "Master Chief High Poly" by HarrisonHag1 (CC-BY-4.0,
https://sketchfab.com/3d-models/master-chief-high-poly-18f5ee1d13904939971601055102c91b), unpacked into
models/master_chief_high_poly/ (not in git). A costume is a whole-body item: the game takes the rest of the outfit
off while one is worn. What this does:
1. Retarget: the model's bones (bip_pelvis, bip_upperArm_L, f_index.01.L, ...) are mapped to Steep's (Reference,
   LeftArm, LeftHandIndex1, ...). Each bone is turned to point along Steep's bone and moved onto Steep's joint (the
   costume's own rest pose, for each gender); the vertices follow by their own weights. Both stand in an A-pose
   facing -Y, so the changes are small. The weights are renamed to Steep's bones.
2. Parts (PRESETS): the model's materials go into the costume's submeshes and texture sets (penguin: all four
   packed 2x2 into the body's; urban_japan: armor, suit and helmet into its own sets); the costume's other
   submeshes (head, goggle, glass) shrink to a speck. Every triangle gets a back face, since the model's materials
   are double-sided and Steep draws front faces only.
3. Out: `mesh.py import --glb` into every LOD of both costumes (bones they lack are added), `texture.py replace`
   for the costume's texture sets (the male and female costume share them).
Needs numpy and Pillow (the textures are JPEG).
"""

import argparse
import os
import re
import shutil
import struct
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

BASE = "DataPC_extra_chr"
# Which costume Chief replaces (--target). A costume's model is picked by the character template's
# STP_M_Clothing / STP_F_Clothing selector from a tag of the costume's item (tools/forge ITEMS notes):
#   penguin     STP_M/F_Costume_005, items JACKET_MF_C_Penguin_228 / COSTUME_MF_PENGUIN_91 (in the store)
#   urban_japan STP_M/F_Costume_016, items COSTUME_MF_URBAN_OUTFIT_JAPAN_139..141 (not in the game's store)
# layout: Steep material -> the model's materials it takes, each with its rectangle of the textures (x, y, w, h in
# UV units, v pointing down as in the upright image); collapse: submeshes shrunk to a speck (by material name, or
# 0x<id> for materials of other datafiles - those are shared with other gear, so their textures stay); colourways:
# per layout material, a pattern of the colourway materials whose texture sets get the same maps.
QUARTERS = [("chiefarmor", (0.0, 0.0, 0.5, 0.5)), ("chiefsuit", (0.5, 0.0, 0.5, 0.5)),
            ("chiefhelmet", (0.0, 0.5, 0.5, 0.5)), ("chiefvisor", (0.5, 0.5, 0.5, 0.5))]
PRESETS = {
    # the military costume (COSTUME_M_MILITARY_GREY_94 / _RED_95) is a gear set: Chief goes into its coat
    # (JACKET_MF_C_Military_233/234 -> STP_M_Jacket_013 / STP_F_Jacket_010), the set's pants, helmet (with the skull
    # mask) and harness shrink (hide); the gloves are the shared STP_MF_Gloves_001 and stay. The coat's embedded
    # material holds the shared placeholder set, its colourways T01/T02 the real ones.
    "military": {"costumes": {"M": "STP_M_Jacket_013", "F": "STP_F_Jacket_010"},
                 "layout": {"STP_MF_Jacket_013_ALL": QUARTERS},
                 "collapse": [], "embedded": False,
                 "colourways": {"STP_MF_Jacket_013_ALL": r"STP_MF_Jacket_013_T\d+"},
                 "hide": ["STP_M_Pants_011", "STP_F_Pants_009", "STP_M_HelmetFun_048", "STP_F_HelmetFun_048",
                          "STP_M_Backpack_002", "STP_F_Backpack_002", "STP_M_BackpackBelt_002_Default",
                          "STP_M_BackpackBelt_002_Large", "STP_M_BackpackBelt_NoBellyStrap_002",
                          "STP_F_BackpackBelt_Default_002", "STP_F_BackpackBelt_Large_002",
                          "STP_F_BackpackBelt_NoBellyStrap_002"],
                 "icons": ["STP_ICON_COSTUME_M_MILITARY_GREY_94_00004_Map", "STP_ICON_COSTUME_M_MILITARY_RED_95_00004_Map"]},
    "penguin": {"costumes": {"M": "STP_M_Costume_005", "F": "STP_F_Costume_005"},
                "layout": {"STP_MF_Costume_005": QUARTERS},
                "collapse": ["STP_MF_Costume_005_HEAD", "0x9c6dd89793", "0x9c6dd897bb"],  # head, goggle, glass
                "colourways": {}, "embedded": True, "hide": [], "icons": ["COSTUME_PENGUIN_MF_Map"],
                # the bundle's extras (shared models, colourways only the penguin's items use): tinted Chief green
                "recolour": ["STP_MF_Backpack_001_FunT25", "STP_MF_Backpack_001_FunT25_BP",
                             "STP_MF_Backpack_001_FunT25_SangleOnly", "STP_MF_Gloves_002_T19",
                             "STP_MF_SkiBoot_001_T19", "STP_MF_SnowboardBoots_001_T19"]},
    "urban_japan": {"costumes": {"M": "STP_M_Costume_016", "F": "STP_F_Costume_016"},
                    "layout": {"STP_MF_Costume_016_Jacket_ALL": [("chiefarmor", (0.0, 0.0, 1.0, 1.0))],
                               "STP_MF_Costume_016_Pants_ALL": [("chiefsuit", (0.0, 0.0, 1.0, 1.0))],
                               "STP_MF_Costume_016_Helmet_ALL": [("chiefhelmet", (0.0, 0.0, 0.5, 1.0)),
                                                                 ("chiefvisor", (0.5, 0.0, 0.5, 1.0))]},
                    "collapse": ["STP_MF_Glass_ALL"], "embedded": True, "hide": [],
                    "icons": ["STP_ICON_COSTUME_MF_URBAN_OUTFIT_JAPAN_139_00006_Map"],
                    "colourways": {f"STP_MF_Costume_016_{p}_ALL": rf"STP_MF_Costume_016_{p}_T\d+"
                                   for p in ("Jacket", "Pants", "Helmet")}},
}
DEFAULT_TARGET = "penguin"
KEYS = ("costumes", "layout", "collapse", "colourways", "embedded", "hide")
ICON_BASE = "DataPC_extra_2"  # the costumes' menu pictures (UI textures, 256 x 256)
TINT = (0.30, 0.36, 0.22)  # Chief's armour green (linear multiplier on a colourway's grey levels)
COSTUMES, LAYOUT, COLLAPSE, COLOURWAYS, EMBEDDED, HIDE = (PRESETS[DEFAULT_TARGET][k] for k in KEYS)
SLOTS = {0: "diffuse", 1: "normal", 2: "specular", 7: "mask"}  # TextureSet slot -> map
FALLBACK = ("Pants", "Glove", "Jacket")  # gear of the same gender searched for bones the costume lacks
SIZE, PAD = 2048, 6 / 2048

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
    """Per joint of the skin: its Steep bone (unmapped joints follow their nearest mapped ancestor), its skinning
    matrix (joint world x inverse bind) and the rest position of every mapped joint (Steep's axes, model units)."""
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
    for k, j in enumerate(joints):
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
    S = np.array(skin).reshape(-1, 4, 4).transpose(0, 2, 1)  # row-major 4x4 per joint
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
            M = np.einsum("vk,vkij->vij", Wt, S[J])
            p = np.einsum("vij,vj->vi", M[:, :3, :3], P) + M[:, :3, 3]
            n = np.einsum("vij,vj->vi", M[:, :3, :3], N)
            out.append({"pos": p[:, [0, 2, 1]] * [1, -1, 1], "nrm": n[:, [0, 2, 1]] * [1, -1, 1],
                        "uv": np.array(g.values(at["TEXCOORD_0"])), "joints": J, "weights": Wt,
                        "tris": np.array(g.values(pr["indices"]), int).reshape(-1, 3),
                        "material": d["materials"][pr["material"]]["name"]})
    return out


# --- Steep's side -----------------------------------------------------------------------------------------------------

def lod0_bones(source: mesh.Source) -> list:
    r = [r for _, r in mesh.mesh_resources(source, ["LOD0"])][0]
    return mesh.parse_mesh(r.payload).bones


def steep_rest(game: str, gender: str, need: set) -> dict:
    """Bone -> (inverse bind matrix, rest matrix): the costume's own, and for bones it lacks the first gear of the
    same gender that has them."""
    arch = os.path.join(game, BASE + ".forge")
    rest = {mesh.bone_name(h): (list(M), gltf.mat_inv_rigid(M))
            for h, M in lod0_bones(mesh.load_source(arch, COSTUMES[gender]))}
    missing = need - set(rest)
    gear = [(fg, e) for fg in (forge.Forge(os.path.join(game, "DataPC_extra_chr_onboarding.forge")), forge.Forge(arch))
            for e in fg.entries if e.type == 0x51DC6B80 and e.name.startswith(f"STP_{gender}_")
            and any(k in e.name for k in FALLBACK)]
    for fg, e in gear:
        if not missing:
            break
        try:
            bones = lod0_bones(mesh.Source(e.name, e.id, e.name, e.type, forge.parse_datafile(fg.read(e)).resources))
        except SystemExit:
            continue
        for h, M in bones:
            if mesh.bone_name(h) in missing:
                rest[mesh.bone_name(h)] = (list(M), gltf.mat_inv_rigid(M))
                print(f"  {COSTUMES[gender]}: {mesh.bone_name(h)} from {e.name}")
        missing = need - set(rest)
    if missing:
        sys.exit(f"{COSTUMES[gender]}: no rest pose for {sorted(missing)}")
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
    sp = {b: np.array(rest[b][1][12:15]) for b in model_pos}
    scale = (sp["Head"][2] - sp["LeftFoot"][2]) / (model_pos["Head"][2] - model_pos["LeftFoot"][2])
    rot: dict = {}

    def rot_of(b):
        if b not in rot:
            c = CHILD.get(b)
            if c in model_pos:
                rot[b] = rotation(model_pos[c] - model_pos[b], sp[c] - sp[b])
            else:  # a leaf (head, finger tips, feet): its parent's rotation
                par = next((p for p, ch in CHILD.items() if ch == b), "Reference")
                rot[b] = rot_of(par) if par != b else np.eye(3)
        return rot[b]
    xf = {}
    for b in model_pos:
        A = rot_of(b) * scale
        if b.endswith("Foot"):
            A = np.diag([1.0, 1.0, sp[b][2] / ((model_pos[b][2] - sole) * scale)]) @ A
        xf[b] = (A, sp[b] - A @ model_pos[b])
    return scale, xf


def texture_sets(game: str) -> dict:
    """Steep material -> [{map: TextureMap id}] of every colourway's TextureSet: the costume's default (embedded in
    its entity) and the colourway materials (`STP_MF_Costume_016_<part>_T01..T03`, entries of their own), which the
    costume's items pick from. Both costumes use the same materials."""
    arch = os.path.join(game, BASE + ".forge")
    s = mesh.load_source(arch, COSTUMES["M"])
    by_name = {r.name: r for r in s.resources}

    def slots(ts):
        return {SLOTS[k]: struct.unpack_from("<Q", ts.payload, 13 + 10 * k + 2)[0]
                for k in SLOTS if ts.payload[13 + 10 * k] == 1}
    fg = forge.Forge(arch)
    out = {}
    for mat in LAYOUT:
        payload = by_name[mat].payload
        sets = [slots(next(r for r in s.resources if r.type == 0xD70E6670 and struct.pack("<Q", r.id) in payload))
                ] if EMBEDDED else []  # without: the embedded set is a shared placeholder
        pattern = COLOURWAYS.get(mat)
        for e in fg.entries if pattern else ():
            if e.type == 0x85C817C3 and re.fullmatch(pattern, e.name):
                newest, entry = mesh.game_entry(arch, f"{e.id:#x}")
                for r in forge.parse_datafile(newest.read(entry)).resources:
                    if r.type == 0xD70E6670 and slots(r) not in sets:
                        sets.append(slots(r))
        out[mat] = sets
    return out


# --- output ---------------------------------------------------------------------------------------------------------

def maps(model: str, out: str) -> dict:
    """Per Steep material its 4 maps from the model's materials, each in its rectangle, row 0 at v = 0 (as the game
    samples UV-mapped textures: texture.py replace --raw-rows). Returns {material: {map: png path}}."""
    tex = os.path.join(model, "textures")
    res = {}
    for mat, parts in LAYOUT.items():
        img = {k: np.zeros((SIZE, SIZE, 4), np.int32) for k in SLOTS.values()}
        for name, (x, y, w, h) in parts:
            ys, xs = slice(round(y * SIZE), round((y + h) * SIZE)), slice(round(x * SIZE), round((x + w) * SIZE))
            pw, ph = xs.stop - xs.start, ys.stop - ys.start

            def load(suffix, fallback):
                p = os.path.join(tex, f"{name}_{suffix}")
                if not os.path.exists(p):
                    return np.broadcast_to(np.array(fallback, np.int32), (ph, pw, 4)).copy()
                return texture.resize(texture.read_image(p), pw, ph)
            img["diffuse"][ys, xs] = load("baseColor.jpeg", (128, 128, 128, 255))
            nm = load("normal.png", (128, 128, 255, 255))
            # X in R, Y in G (pointing up the image, as glTF's), B = 0, A = the jacket's average of its 4th map (G)
            img["normal"][ys, xs] = np.stack([nm[..., 0], nm[..., 1], np.zeros_like(nm[..., 0]),
                                              np.full_like(nm[..., 0], 98)], -1)
            mr = load("metallicRoughness.png", (0, 128, 0, 255))  # glTF: G roughness, B metalness
            spec = np.clip(30 + 0.55 * mr[..., 2] + 0.25 * (255 - mr[..., 1]), 0, 255)
            img["specular"][ys, xs] = np.stack([spec, spec, spec, np.full_like(spec, 255)], -1)
            # Mask1 R: the jackets have it white on fabric; in the game white turned the model's dark suit pale
            # cream (G: frost/snow on cloth), so every part is "hard" (0)
            img["mask"][ys, xs] = (0, 0, 255, 255)
        res[mat] = {}
        for k, v in img.items():
            res[mat][k] = os.path.join(out, f"{mat}_{k}.png")
            texture.write_png(res[mat][k], v)
    return res


def write_glb(parts: list, xf: dict, rest: dict, path: str) -> tuple[int, int]:
    """The fitted body, skinned to Steep's bones: a primitive per Steep material (LAYOUT), with a back face for
    every triangle, plus a speck (one 1 mm triangle inside the chest) for each COLLAPSE material."""
    target = {name: (mat, rect) for mat, lst in LAYOUT.items() for name, rect in lst}
    bones = sorted({b for p in parts for b in p["bones"]} | {"Spine2"})
    bi = {b: i for i, b in enumerate(bones)}
    nmat = {k: np.linalg.inv(A).T for k, (A, _) in xf.items()}  # normals: the inverse transpose
    prims: dict[str, dict] = {}
    for p in parts:
        mat, (x, y, w, h) = target[p["material"]]
        q = prims.setdefault(mat, {"pos": [], "nrm": [], "uv": [], "js": [], "ws": [], "idx": []})
        base = len(q["pos"])
        for k in range(len(p["pos"])):
            v, n = p["pos"][k], p["nrm"][k]
            infl: dict = {}
            for j, wt in zip(p["joints"][k], p["weights"][k]):
                if wt > 0:
                    infl[p["bones"][j]] = infl.get(p["bones"][j], 0.0) + float(wt)
            top = sorted(infl.items(), key=lambda kv: -kv[1])[:4]
            tot = sum(wt for _, wt in top)
            vp = sum(wt * (xf[b][0] @ v + xf[b][1]) for b, wt in top) / tot
            vn = sum(wt * (nmat[b] @ n) for b, wt in top)
            q["pos"].append(vp)
            q["nrm"].append(vn / (np.linalg.norm(vn) or 1))
            u, t = (min(max(float(c), 0.0), 1.0) for c in p["uv"][k])  # the model's UVs stay within 0..1
            q["uv"].append((x + PAD + u * (w - 2 * PAD), y + PAD + t * (h - 2 * PAD)))
            q["js"].append(tuple(bi[b] for b, _ in top) + (0,) * (4 - len(top)))
            q["ws"].append(tuple(wt / tot for _, wt in top) + (0.0,) * (4 - len(top)))
        q["idx"] += [base + int(i) for i in p["tris"].reshape(-1)]
    chest = np.array(rest["Spine2"][1][12:15])
    for mat in COLLAPSE:
        prims[mat] = {"pos": [chest, chest + [0.001, 0, 0], chest + [0, 0, 0.001]], "nrm": [np.array([0, -1.0, 0])] * 3,
                      "uv": [(0.5, 0.5)] * 3, "js": [(bi["Spine2"], 0, 0, 0)] * 3, "ws": [(1.0, 0, 0, 0)] * 3,
                      "idx": [0, 1, 2]}
    b = gltf.Builder("powder mods/master_chief/make_chief.py")
    root = b.add("nodes", {"name": "Steep_Rig"})
    jn = [b.add("nodes", {"name": name, **gltf.trs(gltf.mat_to_gl(rest[name][1]))}) for name in bones]
    b.doc["nodes"][root]["children"] = jn
    ibm = b.accessor([gltf.mat_to_gl(rest[name][0]) for name in bones], "MAT4", target=None)
    skin = b.add("skins", {"joints": jn, "inverseBindMatrices": ibm, "skeleton": root})
    out, nv, nt = [], 0, 0
    for mat, q in prims.items():
        P, N, T = np.array(q["pos"]), np.array(q["nrm"]), np.array(q["idx"]).reshape(-1, 3)
        if mat not in COLLAPSE:  # turn triangles wound against their normals, then add the back faces
            fn = np.cross(P[T[:, 1]] - P[T[:, 0]], P[T[:, 2]] - P[T[:, 0]])
            flip = (fn * N[T].sum(1)).sum(1) < 0
            T[flip] = T[flip][:, ::-1]
            n = len(P)
            P, N = np.concatenate([P, P]), np.concatenate([N, -N])
            q["uv"], q["js"], q["ws"] = q["uv"] * 2, q["js"] * 2, q["ws"] * 2
            T = np.concatenate([T, T[:, ::-1] + n])
        nv, nt = nv + len(P), nt + len(T)
        at = {"POSITION": b.accessor([gltf.to_gl(x) for x in P.tolist()], "VEC3", bounds=True),
              "NORMAL": b.accessor([gltf.to_gl(x) for x in N.tolist()], "VEC3"),
              "TEXCOORD_0": b.accessor(q["uv"], "VEC2"), "JOINTS_0": b.accessor(q["js"], "VEC4", gltf.USHORT),
              "WEIGHTS_0": b.accessor(q["ws"], "VEC4")}
        out.append({"attributes": at, "material": b.add("materials", {"name": mat}),
                    "indices": b.accessor(T.reshape(-1).tolist(), "SCALAR", gltf.UINT,
                                          target=gltf.ELEMENT_ARRAY_BUFFER)})
    node = b.add("nodes", {"name": "master_chief", "skin": skin,
                           "mesh": b.add("meshes", {"name": "master_chief", "primitives": out})})
    b.doc["scenes"][0]["nodes"] += [root, node]
    with open(path, "wb") as f:
        f.write(b.glb())
    return nv, nt


def picture(game: str, name: str, png: str, out: str) -> None:
    """The costume's menu picture: the render over the game's own icon's backdrop. The menu draws an icon's colours
    and ignores its alpha (the shipped icons hide a backdrop under alpha 0), so the old costume is painted out with
    the backdrop's colour (smoothed) and the render blended onto that."""
    from PIL import Image, ImageFilter
    orig = os.path.join(os.path.dirname(out), "orig.png")
    run(os.path.join(TOOLS, "texture.py"), "export", os.path.join(game, ICON_BASE + ".forge"), name, "-o", orig)
    o = np.asarray(Image.open(orig).convert("RGBA"), dtype=np.float32)
    clear = o[..., 3] == 0
    fill = o[..., :3][clear].mean(axis=0) if clear.any() else np.array([190.0, 210, 230])
    back = np.where(clear[..., None], o[..., :3], fill)
    back = Image.fromarray(back.astype(np.uint8), "RGB").filter(ImageFilter.GaussianBlur(6)).convert("RGBA")
    top = Image.open(png).convert("RGBA").resize(back.size)
    rgb = Image.alpha_composite(back, top)
    rgb.putalpha(top.getchannel("A"))
    rgb.save(out)


def recolour(game: str, names: list[str], tmp: str, out: str) -> None:
    """Tint the diffuse maps of these colourway materials (entries of TextureSets) Chief's green, keeping their
    shading: the bundle's gear that shares its model with other items can't be hidden, but its colours are its own."""
    from PIL import Image
    arch = os.path.join(game, BASE + ".forge")
    for name in names:
        newest, entry = mesh.game_entry(arch, name)
        ids = {struct.unpack_from("<Q", r.payload, 15)[0] for r in forge.parse_datafile(newest.read(entry)).resources
               if r.type == 0xD70E6670 and r.payload[13] == 1}  # slot 0: the diffuse map
        for tid in ids:
            src = os.path.join(tmp, f"{tid:x}.png")
            run(os.path.join(TOOLS, "texture.py"), "export", arch, f"{tid:#x}", "-o", src)
            im = Image.open(src).convert("RGBA")
            grey = np.asarray(im.convert("L"), dtype=np.float32) / 255
            rgb = np.clip(grey[..., None] * np.array(TINT) * 255 * 1.6, 0, 255).astype(np.uint8)
            tinted = Image.fromarray(rgb, "RGB")
            tinted.putalpha(im.getchannel("A"))
            tinted.save(src)
            for base in sorted({b for b, _ in holders(game, [tid])}):
                run(os.path.join(TOOLS, "texture.py"), "replace", os.path.join(game, base + ".forge"), f"{tid:#x}",
                    "--image", src, "-o", os.path.join(out, base))
        print(f"  tinted: {name} ({len(ids)} map{'s' if len(ids) != 1 else ''})")


def holders(game: str, ids: list[int]) -> set[tuple[str, str]]:
    """(base archive, entry id) of every datafile that holds one of these resources."""
    out = run(os.path.join(TOOLS, "forge.py"), "where", game, *[x for i in ids for x in ("--id", f"{i:#x}")])
    return {(m[2], m[1]) for m in re.finditer(r"entry (0x[0-9a-f]+) .*\(base (\S+)\)", out)}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--game", required=True, help="the Steep folder")
    ap.add_argument("--model", default=os.path.join(HERE, "..", "..", "models", "master_chief_high_poly"))
    ap.add_argument("-o", "--out", default=os.path.join(HERE, "client", "forge"))
    ap.add_argument("--glb", help="also keep the fitted models, <this>_M.glb and <this>_F.glb (to look at in Blender)")
    ap.add_argument("--target", choices=sorted(PRESETS), default=DEFAULT_TARGET, help="the costume Chief replaces")
    ap.add_argument("--icon", help="the costume's menu picture (PNG, e.g. from render_icon.py)")
    a = ap.parse_args()
    global COSTUMES, LAYOUT, COLLAPSE, COLOURWAYS, EMBEDDED, HIDE
    COSTUMES, LAYOUT, COLLAPSE, COLOURWAYS, EMBEDDED, HIDE = (PRESETS[a.target][k] for k in KEYS)
    g = gltf.load(os.path.join(a.model, "scene.gltf"))
    steep, skin, mpos = model_bones(g)
    parts = model_geometry(g, skin)
    for p in parts:
        p["bones"] = steep
    sole = min(float(p["pos"][:, 2].min()) for p in parts)
    tmp = tempfile.mkdtemp(prefix="powder_chief_")
    try:
        shutil.rmtree(a.out, ignore_errors=True)
        for gender, costume in COSTUMES.items():
            rest = steep_rest(a.game, gender, set(mpos) | {"Spine2"})
            scale, xf = fit(mpos, rest, sole)
            glb = os.path.join(tmp, f"chief_{gender}.glb")
            nv, nt = write_glb(parts, xf, rest, glb)
            print(f"{costume}: {nv} vertices, {nt} triangles (with back faces), {scale:.6f} m per model unit")
            if a.glb:
                shutil.copy(glb, f"{a.glb}_{gender}.glb")
            lods = [r.id for _, r in mesh.mesh_resources(mesh.load_source(os.path.join(a.game, BASE + ".forge"),
                                                                          costume), None)]
            for base, entry in sorted(holders(a.game, lods)):
                res = run(os.path.join(TOOLS, "mesh.py"), "import", os.path.join(a.game, base + ".forge"), entry,
                          "--glb", glb, "--to", "LOD", "--ignore-names", "-o", os.path.join(a.out, base))
                print("\n".join(line for line in res.splitlines() if "submesh" in line))
        pngs = maps(a.model, tmp)
        sets = texture_sets(a.game)
        where = holders(a.game, [t for lst in sets.values() for m in lst for t in m.values()])
        for mat, lst in sets.items():
            for n, ids in enumerate(lst):
                for k, tid in ids.items():
                    for base in sorted({b for b, e in where if int(e, 16) == tid}):
                        run(os.path.join(TOOLS, "texture.py"), "replace", os.path.join(a.game, base + ".forge"),
                            f"{tid:#x}", "--image", pngs[mat][k], "--raw-rows", "-o", os.path.join(a.out, base))
                print(f"  {mat}: colourway {n + 1} of {len(lst)}, {len(ids)} maps")
        # the rest of a gear-set costume, shrunk to a point wherever its meshes are
        arch = os.path.join(a.game, BASE + ".forge")
        def mesh_ids(name):
            return [r.id for _, r in mesh.mesh_resources(mesh.load_source(arch, name), None)]
        mine = holders(a.game, [i for c in COSTUMES.values() for i in mesh_ids(c)]) if HIDE else set()
        for name in HIDE:
            for base, entry in sorted(holders(a.game, mesh_ids(name)) - mine):
                run(os.path.join(TOOLS, "mesh.py"), "edit", os.path.join(a.game, base + ".forge"), entry,
                    "--scale", "0.001", "-o", os.path.join(a.out, base))
            print(f"  hidden: {name}")
        recolour(a.game, PRESETS[a.target].get("recolour", []), tmp, a.out)
        for name in PRESETS[a.target]["icons"] if a.icon else []:
            pic = os.path.join(tmp, "picture.png")
            picture(a.game, name, a.icon, pic)
            run(os.path.join(TOOLS, "texture.py"), "replace", os.path.join(a.game, ICON_BASE + ".forge"), name,
                "--image", pic, "-o", os.path.join(a.out, ICON_BASE))
            print(f"  picture: {name}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
