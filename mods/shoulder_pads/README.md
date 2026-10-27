# shoulder_pads: new geometry from a glTF (Blender) file

This mod shows that the game draws **new geometry with its own bone weights** (confirmed in-game). It adds two orange shoulder pads
to the default male rider's jacket, `STP_M_Jacket_001`, which the menu rider (Freerider M, item `JACKET_M_N_001`)
wears. The jacket keeps every vertex it had. Each pad is a flattened ellipsoid of 220 vertices and 400 triangles,
added to the jacket's submesh in all 4 LODs and bound 100 % to the upper-arm helper bone of its side
(`L_LeftArm_NoRoll`, `L_RightArm_NoRoll`), so it should follow the arm.

| replaced entry (keeps its id) | archive (base) | holds | what changes |
|---|---|---|---|
| `0x8b66b1fe21` `STP_M_Jacket_001` (entity) | `DataPC_extra_chr_onboarding` | `_LOD0` .. `_LOD3`, material, texture set | +440 vertices, +800 triangles in each LOD |
| `0x7a5374f895` `STP_M_Jacket_001_LOD0` | `DataPC_STP_Alpes` | the LOD0 mesh, material, texture set | the same for LOD0 |

These are all the datafiles that hold one of the four LOD meshes (`tools/forge/forge.py where`). `mods/mesh_edit`
replaces the same two entries, so enable only one of the two. With both, building the session's
`_patch_03` fails on the duplicate ids, and powder-wrap logs that and leaves the archive out.

## What you should see

In the main menu, Freerider M's jacket has a rounded orange pad on top of each shoulder. Each pad is about 17 cm
long, 19 cm front to back and 2-3 cm proud of the jacket. The pads follow the shoulders when the rider moves. The
rest of the jacket looks as before. The pads are plain orange: every UV points at one orange texel of the jacket's
own diffuse map.

If the jacket looks unchanged, check that the session put `DataPC_extra_chr_onboarding_patch_03.forge` beside its
base (powder-wrap's log says "asset mods: ..."). Since the same archive path showed `mods/mesh_edit`, a missing
pad points at the geometry. If the game crashes at the menu, the rebuilt mesh is the suspect: the vertex and index
buffers changed size. Turn the mod off and report it.

## How it was made

`model/pads.glb` is the mod's own geometry, with nothing of the game in it. `model/make_pads.py` writes it, the
way a Blender user would model it:
- two objects, `pad_L@L_LeftArm_NoRoll` and `pad_R@L_RightArm_NoRoll`. The `@Bone` suffix binds an unskinned
  object rigidly to that bone. In Blender, *Parent to Bone* does the same;
- the material is named `STP_M_Jacket_001_ALL`, the jacket's, so the pads join the jacket's submesh;
- Steep's space: metres, Z up, the rider faces -Y (left is +X), the shoulder tops at x = +-0.2 m, z = 1.53 m.

`generate.sh` adds them to the jacket from your install, without exporting the jacket itself:

```
python3 tools/forge/mesh.py import "$G/DataPC_extra_chr_onboarding.forge" 0x8b66b1fe21 \
    --glb mods/shoulder_pads/model/pads.glb --to LOD --add -o mods/shoulder_pads/client/forge/DataPC_extra_chr_onboarding
python3 tools/forge/mesh.py import "$G/DataPC_STP_Alpes.forge" 0x7a5374f895 \
    --glb mods/shoulder_pads/model/pads.glb --to LOD0 --add -o mods/shoulder_pads/client/forge/DataPC_STP_Alpes
```

`--add` keeps the jacket's own vertices byte for byte and appends the pads. `--to LOD` puts objects that name no
mesh into every LOD. To make your own version in Blender:
1. `mesh.py export ... -o jacket.glb --textures` and import that into Blender, for reference;
2. import `pads.glb` too, reshape the pads (or model something else) and parent or name it to a bone;
3. export only the new objects (*Selected Objects*) as .glb, and use that file in `generate.sh`.

`tools/forge/BLENDER.md` has the whole workflow.

## Verified vs assumed

Verified with tools, without running the game:
- After the import, all 5854 (LOD0) / 2939 / 1272 / 226 original vertices of the jacket are byte for byte the
  same. The 440 new vertices per LOD carry `L_LeftArm_NoRoll` or `L_RightArm_NoRoll` at 255. Both bones were in
  every LOD's bone map already, so the bone lists, bone maps and the position/UV scales are unchanged.
- Every rebuilt mesh passes `mesh.py check`. Exported to glb and rendered in Blender with the jacket's texture,
  the pads sit on both shoulders, orange, with outward normals.

Assumed (this mod is the test):
- The engine accepts vertex/index buffers of a new size (the bone list and maps are unchanged here; a mod that
  adds a bone would be the next test).
- The second vertex copy (section 2) may be written in the same order as the first (shipped data reorders it).

## Regenerating

The jacket entries are Steep's data and not in git (`/mods/*/client/forge/` is ignored). Run
`mods/shoulder_pads/generate.sh [<Steep folder>]` once before you enable the mod. `model/make_pads.py` rewrites
`model/pads.glb` (it is committed).

## Undo

Turn the mod off: leave it out of `POWDER_CLIENT_MODS`, or decline it in the launcher. The installed game files and
the save are never changed.
