# Steep's meshes (type 0x415D9568)

`mesh.py` reads Steep's mesh resources, exports them as OBJ, moves their vertices, and imports OBJs back. It
writes unpacked datafile folders, like `forge.py extract` and `swap` do, and `forge.py build` packs those into a
`_patch_03`. It uses `forge.py` from the same folder and needs only the standard library (Python 3.10+). Archives
are only read.

```
mesh.py info   <src> [entry] [--mesh LOD0]                     # layout: formats, counts, bounds, submeshes, skinning
mesh.py export <src> [entry] -o objs/ [--skin]                 # one OBJ per LOD and submesh (+ bones/weights JSON)
mesh.py edit   <src> [entry] -o out/ --inflate 0.05 | --scale 1.2[,sy,sz] [--pivot x,y,z] | --translate x,y,z
mesh.py import <src> [entry] --obj <mesh>_sm<k>.obj ... -o out/ [--topology]
mesh.py check  <archive> [-g name] [--limit n]                 # parse every mesh, identity rewrite, sanity checks
```

`<src>` can be an unpacked folder, a `.datafile`, a bare `.payload`, or `ARCHIVE.forge ENTRY`. With an archive,
the entry comes from the newest archive of its stack, as the game loads it. Archives that a powder-wrap session
put there (listed in `.powder-assets`) are skipped, so a mod's own output is never edited a second time. By default,
`edit` and `import` act on every mesh resource of the datafile, so all LODs are covered. `--mesh` narrows that.

```
$ mesh.py info "$G/DataPC_extra_chr_onboarding.forge" 0x8b66b1fe21 --mesh LOD0
DataPC_extra_chr_onboarding.forge:STP_M_Jacket_001: entry 0x8b66b1fe21 STP_M_Jacket_001, 7 resource(s)
STP_M_Jacket_001_LOD0  id=0x7a5374f895  635106 bytes  skinned, byte 0x0D = 1
  bounds    min (-0.549, -0.163, 0.857)  max (0.549, 0.197, 1.658) m  (size 1.098 x 0.361 x 0.801)
  scales    positions S = 2 m, UVs X = 1.05548
  format    2, stride 44: pos i16x4 @0 (x S/32767), normal u8x4 @8, tangent u8x4 @12, binormal u8x4 @16, uv0 i16x2 @20,
            uv1 i16x2 @24, bones u8x4 @32, weights u8x4 @36, unknown @28, @40
  section 1 5854 vertices, 28566 indices (u16, relative to each submesh's first vertex)
  section 2 5854 vertices, 28566 indices (u16, absolute), the same vertices as section 1 (reordered in shipped data)
  bones     62: name hashes 07c159a2, fe5fcc55, 655b902f, e01ef398, eb830ada, 2d4660a8, ...
  submeshes 1:
    0: vertices 0..5853 (5854), 9522 triangles, material 0x7a5374f6c4 STP_M_Jacket_001_ALL, bone map 62
  decoded   min (-0.549, -0.163, 0.857)  max (0.549, 0.197, 1.658)
  skinning  654/395/536/4269 vertices with 1/2/3/4 bones (u8 bone slots into the submesh's bone map, u8 weights summing to 255)
```

**C** = confirmed: it holds on every mesh checked, or on the samples stated. **G** = a guess, or an observation
whose meaning is unknown. "Sampled" means 694 meshes: every mesh in `DataPC_extra_chr_onboarding` and
`DataPC_STP_WhiteRoom`, plus the first 400 in `DataPC_STP_Alpes` (characters, gear, buildings, rocks, vegetation,
terrain). `mesh.py check` over all 27 archives parsed every mesh. The results are at the end of this file.

## Where meshes live

- Each LOD is a mesh resource of its own (`*_LOD0` .. `*_LOD3`), with its own bones, buffers and scales (C).
- A character item is an **entity** datafile (type 0x51DC6B80, e.g. `STP_M_Jacket_001`). It holds the entity
  (207 bytes: the LOD mesh ids plus floats 15, 30, 80, 160, G: LOD distances), its 4 LOD meshes, the default
  material and the material's TextureSet, all inline (C).
- Resources are duplicated across datafiles. `STP_M_Jacket_001_LOD0` (0x7a5374f895) is in the onboarding entity
  *and* is an entry of its own in `DataPC_STP_Alpes`. LOD1..3 are only in the entity. This was checked over the
  header blocks of all 164,970 datafiles. The two LOD0 copies differ in one byte (header +0x0D: 1 vs 2, G).

## Layout

All little-endian. Nested objects are serialised as `u64 id (0 here), u32 class hash, body` (C).

| part | contents |
|---|---|
| header | u64 resource id, u32 0x415D9568, u8 1, u8 flag (G: 0 on sampled static meshes, 1 or 2 on skinned ones, 2 on the Alps terrain), 9 zero bytes |
| bones @0x17 | u32 count, then count x 81 bytes: u64 0, u32 class 0x9EF0E7A1, u32 bone name hash, f32[16] inverse bind matrix (column-major, translation in [12..14]), u8 0 |
| bounds | f32 min x,y,z, 0, max x,y,z, 0 (metres, Z up) |
| | u8 0 |
| compiled mesh | u64 0, u32 class 0xFC9E1595, u32 0, u8 (0 if section 1 follows, 3 if not; C on the samples) |
| section 1 (optional) | u64 0, u32 class 0xC351EE43, u32 1, u8 vertex format, u32 stride, u32 0, f32 centre[3], f32 half size[3], u32 n (submeshes), u32 n + n x u32 (always 1, G), u32 n + n x u32 (each submesh's first vertex x stride / 4: its dword offset in the vertex buffer, C), u8 0, **u32 bytes + vertex buffer**, **u32 bytes + index buffer** (u16, relative to the submesh's first vertex, C), u32 36n + n records |
| section 1 record | f32 centre[3], f32 half size[3] (the submesh's bounds), u32 stride, u32 (index count << 8 \| 1), u32 first index (C: 915/915 sampled records match the draw ranges) |
| section 2 | u64 0, u32 class 0x0645ABB5, u8 32-bit indices (C on the 2 meshes with 1: the helicopter and the Alps terrain, both without section 1), u8 vertex format, u8 stride, u32 k + k draw ranges, u32 k' + k' draw ranges, **u32 bytes + vertex buffer**, **u32 bytes + index buffer** (absolute indices) |
| draw range | u64 0, u32 class 0xA57387EF, u32 first vertex, u32 0, u32 vertex count, u32 first index, u32 triangle count, u32 1 |
| submesh table | u32 n, then n x (u64 0, u32 class 0x0B1D34C1, 18 bytes, u16[128] bone map with unused slots 0xF7F7) |
| submesh, 18 bytes | u8 1 (one sampled 0), u8 0/1, u8 0, u16 bone-map length, u16 draw-range index, u24 vertex count (G: matches the draw range on most, not on all), u64 material id |
| scales | u32 26, u32 7 (C: every mesh), **f32 S** (position scale, whole metres), **f32 X** (UV scale; 0 = no UVs) |
| material slots | u32 n, n x (u64 0, u32 class 0x9F1640AF, u8 1, u8 0 or 4, u64 material id, u8 0, u64 material id, u64 0), then a few bytes ending in a u32 (0x15, 0x18, 0x24 seen) (G, not needed for vertex work) |

Sections and draw lists:
- **Section 1** holds the buffers the submeshes are drawn from, with base-vertex indices. If it is missing (102
  sampled: vegetation, the terrain, the parachute and the helicopter), section 2 holds the only copy.
- **Section 2's vertex buffer, when both sections have one, is a reordering of section 1's**. That holds in 164/164
  sampled meshes and in every mesh `check` saw. Section 2 is often empty (e.g. `STP_M_Goggle_001_LOD0`).
- The first draw list is per submesh. The second is, in the sampled meshes, one of these:
  - a copy of the first (563);
  - a subset of it, possibly empty (35; e.g. the helicopter ski box leaves out 1 of its 5 submeshes; G: those
    that cast no shadow);
  - one range over all submeshes (72);
  - other ranges (24: static props and 3 parachute LODs; G: some cover more vertices than the file holds).

### Vertex formats (C: stride and statistics on all 694 sampled meshes)

| format | stride | layout | sampled |
|---|---|---|---|
| 0 | 32 | pos i16x4, normal, tangent, binormal (u8x4 each), uv0 i16x2 @20, bones u8x4 @24, weights u8x4 @28 | 131 skinned |
| 1 | 40 | pos, normal, tangent, binormal, uv0 @20, uv1 @24, @28..39 unknown (G: a third i16x2 pair, weights, bones) | 3 heads |
| 2 | 44 | pos, normal, tangent, binormal, uv0 @20, uv1 @24, @28 always 0, bones @32, weights @36, @40 always ff 80 ff ff | 113 skinned (jacket) |
| 3 | 52 | pos, normal, tangent, binormal, uv0 @20, uv1 @24, rest unknown | 1 scarf |
| 6 | 24 | pos, normal @8, tangent @12, colour u8x4 @16 (G), uv0 @20 | 112 static |
| 8 | 20 | pos, normal @8, tangent @12, uv0 @16 | 334 static |

- **Positions** are i16x4. Skinned (0..3): `xyz * S / 32767`, and w is a per-vertex value unrelated to the
  position (on the jacket, multiples of 32767/255 from 19917 to 32767, G: an 8-bit cloth/wind weight).
  Static (6, 8): `xyz * w / 32767`, with `w = +S` or `-S` per vertex; the sign is part of the encoding, since
  decoding without it mirrors the mesh (C: the decoded positions span the stored bounds on every mesh checked,
  and with this rule the normals agree with the face winding). S is a whole number of metres in every sampled
  mesh (1..8003).
- **Normal, tangent, binormal**: u8x4, `(b - 127.5) / 127.5` per component. The w byte is 255 on skinned meshes;
  on static meshes the tangent's w is 128 or 255 (G: handedness). The face normal `cross(p1 - p0, p2 - p0)` agrees
  with the stored normals: the median dot product per format is 0.98-0.998, so triangles are counter-clockwise
  seen from the front, as OBJ expects (C). Tangent = +dP/du and binormal = -dP/dv, with v pointing down (C: 236
  skinned meshes compared with UV-gradient frames, mean dot per mesh +0.6..+1.0 and -0.6..-1.0, mostly +-0.9..1).
- **UVs**: i16x2, `* X / 32767`, with v pointing down (D3D). When X > 0, the largest |value| over the UV columns
  (uv0 and uv1 together) is exactly 32767, so X is the range the UVs were normalised to (C on all sampled meshes;
  fmt 2 meshes reach it in uv1 when uv0 does not). Checked against the texture: the jacket's UV island borders,
  drawn over its decoded 512x512 BC1 diffuse map, follow the painted panels exactly (C).
- **Skinning**: up to 4 influences. `bones` are u8 slots into the submesh's bone map, the map entry is an index
  into the mesh's bone list, and `weights` are u8 summing to 255 (C: every sampled skinned vertex). The bone map
  matters: on jacket LOD3 the map skips bone 29. Vertices bound to a single bone sit a median of 8 cm (LOD0) or
  15 cm (LOD3) from that bone's origin when the matrix is read column-major as inverse bind. The other readings
  give 0.5-3 m (C).
- **Bounds**: the header box and section 1's whole-mesh box are the source positions' bounds. They match the
  decoded positions to within 1.5 quantisation steps (C, 592 sampled with section 1). Most per-submesh records
  match their submesh's vertices the same way; a few do not (G).

## Writing meshes back

`edit` and same-topology `import` keep the vertex and index counts. They change only the bytes that move:

- the main buffer's positions (other bytes untouched), and the same rows in section 2's copy, matched by their
  old bytes;
- the header bounds, section 1's centre and half size, and each submesh record's box (recomputed from the decoded
  new positions);
- S, if a position no longer fits. S becomes the next whole metre, and every position is encoded again. On static
  meshes each vertex's w becomes +-S.

A vertex whose position is unchanged keeps its bytes, so **an identity edit returns the payload byte for byte**.
`check` runs that on every mesh, and also re-encodes every decoded position (C, every mesh of all 27 archives). On
the jacket entity:
- an export followed by an import of the same OBJs reproduces all four LOD payloads byte for byte;
- importing the OBJs of an `--inflate 0.05` edit gives exactly the payloads that `edit --inflate 0.05` wrote.

`--inflate` moves each vertex along its normal. The normal is averaged over all vertices at the same position, so
UV and hard-edge seams move together and do not open. Where the copies face apart (both sides of a thin sheet),
each copy keeps its own normal: that is 18 of 4823 positions on jacket LOD0. Normals and tangents are not
changed. That is right for inflate (an offset surface has the same normals), translate and uniform scale. A
non-uniform scale leaves them slightly off.

Import (same topology): OBJ vertex i is submesh vertex i, and the faces must match. A position, UV or normal that
rounds to the stored value keeps its bytes. A normal that turned by more than 1 degree is stored, and the tangent
and binormal are re-orthogonalised against it. UVs outside +-X are refused (X stays as it is).

**`import --topology`** (vertex formats 0 and 2) takes OBJs with any vertex and face counts, one per submesh, and
rebuilds the payload:
- one vertex per distinct `v/vt/vn` corner;
- normals from `vn`, or area-weighted from the faces;
- tangent and binormal from the UV gradients (the convention above);
- everything an OBJ cannot carry is taken from the **nearest original vertex of the same submesh**: bone slots and
  weights, uv1, the position's w, and the unknown bytes.

It rewrites both sections' buffers, the draw lists, the section 1 offsets and records, the submesh table's
vertex counts, the bounds, S and X. X grows to cover the new UVs, and uv1 is encoded again with it. Up to 65535
vertices per submesh are allowed (16-bit relative indices), and in total where section 2 holds a copy. The rebuilt
payload passes `check`. Tested on the jacket: LOD0 subdivided (5854 -> 21165 vertices, 9522 -> 38088 triangles),
LOD0 replaced by LOD3's geometry (226 vertices), and the unchanged OBJ (the same triangles, positions, UVs and
skinning; only the normals' bytes differ, because they are renormalised). Also tested on a goggle submesh (empty
section 2) and a ski-box submesh (subset draw list). The archives built from these pass `forge.py verify
--decompress`. **The game has loaded none of this yet.**

## What a fully new model still needs

The geometry side works: `import --topology` produces a structurally valid mesh with any topology. Still to do:

1. **Real skin weights.** Nearest-vertex transfer is right for shapes close to the original surface (a reshaped
   jacket, a hood) and wrong for anything else, such as a cape far from the body. A new model needs its own
   weights for the skeleton's bones. `export --skin` writes the bones (name hash, inverse bind matrix), the bone
   map and the per-vertex slots and weights. The missing parts are a Blender-side importer/exporter (an armature
   from the inverse bind matrices, vertex groups named by hash) and an `import` that reads weights instead of
   transferring them.
2. **The rider skeleton.** Bones are known only by u32 name hashes and their bind poses inside each mesh. The
   skeleton resource with the hierarchy and names is not decoded. Reusing an existing garment's bone list (the
   jacket's 62 bones cover the torso, arms and head) is the practical route.
3. **Materials and textures.** New UVs need a texture painted for them. TextureMap/Mip writing is
   `tools/forge/texture.py`'s job. A new material means a new Material + TextureSet, or a `swap` of an existing
   one.
4. **All LODs.** Each LOD is its own mesh. Either build four, or reuse one geometry four times.
5. **New ids.** Today a model replaces an existing id. A brand-new item needs a new entity, a selector entry in
   the character template (`STP_MF_Template`, see `mods/model_swap/README.md`), an item in Game Bootstrap
   Settings and prefetch entries.
6. **Unknowns that may matter**:
   - the per-vertex w on skinned positions (cloth/wind?);
   - format 2's @28/@40 and formats 1/3 beyond uv1;
   - the submesh table's u24 where it does not match;
   - the material-slot block;
   - draw lists of the "other" form;
   - whether the engine checks section 2 against section 1. Section 2 is written as a plain copy, not
     reordered.
7. **An in-game test** of a changed mesh (`mods/mesh_edit`), first with the same topology, then a rebuilt one.

## Results of `mesh.py check` (all 27 archives, 2026-10-26)

`check` reads the mesh resources inside Mesh and Entity entries; meshes inside other entry types are not
covered. Per mesh it checks these:
- it parses: every structure above, including the scale fields 26/7;
- its triangles stay inside the vertex buffer;
- an identity rewrite returns its bytes unchanged;
- every position re-encodes to itself;
- section 2 is a reordering of section 1;
- the decoded positions span the stored bounds to within two quantisation steps.

| archives | meshes | problems |
|---|---|---|
| `DataPC_extra_chr` + `_patch_01` + `_patch_02` | 2216 + 746 + 1724 | 0 |
| `DataPC_extra_chr_onboarding` + patches | 240 + 112 + 87 | 0 |
| `DataPC` + patches | 149 + 136 + 8 | 0 |
| `DataPC_STP_Alaska` + patches | 1684 + 1303 + 10 | 0 |
| `DataPC_STP_Alpes` + patches | 2609 + 23 + 30 | 0 |
| `DataPC_STP_Japan` + patches | 3244 + 134 + 8 | 0 |
| `DataPC_STP_WhiteRoom` | 61 | 0 |
| `DataPC_extra`, `DataPC_extra_2` (+ patches) | 0 | - |
| **total** | **14,524** (per archive; ids repeat across archives) | **0** |
