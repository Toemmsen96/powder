# Models from Blender

`mesh.py` moves Steep's meshes to and from Blender as glTF (`.glb`). The file carries the armature, the per-vertex
bone weights, the UVs and the materials, with the diffuse texture embedded if you ask for it. Blender's built-in
glTF importer and exporter read and write it, so no add-on is needed. You can reshape an item, model a new one
over the rider's skeleton, weight-paint it, and turn the result back into a mesh resource for an asset mod.

Tested with Blender 4.5.9 LTS, 5.0.1 (the `bpy` module) and 5.2.2, all headless (`blender --factory-startup -b
--python script.py -- args`): the results below are the same in all three.

```
G=~/Games/ubisoft-connect/drive_c/Program\ Files\ \(x86\)/Ubisoft/Ubisoft\ Game\ Launcher/games/Steep
M=tools/forge/mesh.py

# 1. out of the game
python3 $M export "$G/DataPC_extra_chr_onboarding.forge" STP_M_Jacket_001 -o jacket.glb --textures
#    (--mesh LOD0: only one LOD; --rig STP_M_Pants_001: add that item's bones, e.g. the legs, to the armature)

# 2. Blender: File > Import > glTF 2.0, edit, File > Export > glTF 2.0 (.glb) -> jacket_new.glb

# 3. back into the game: every datafile that holds the mesh (forge.py where finds them)
python3 tools/forge/forge.py where "$G" --id 0x7a5374f895
python3 $M import "$G/DataPC_extra_chr_onboarding.forge" STP_M_Jacket_001 --glb jacket_new.glb \
    -o mods/my_jacket/client/forge/DataPC_extra_chr_onboarding
python3 $M import "$G/DataPC_STP_Alpes.forge" 0x7a5374f895 --glb jacket_new.glb \
    -o mods/my_jacket/client/forge/DataPC_STP_Alpes
```

Then add a `mod.toml` and enable the mod (`POWDER_CLIENT_MODS=my_jacket` with powder-wrap). The session builds
`<Base>_patch_03.forge` from `client/forge/<Base>/`, and the game loads the newest copy of each entry
(`native/sdk/README.md`, "Asset mods"). `mods/shoulder_pads` is a worked example.

## The Blender add-on (no terminal needed)

`tools/forge/powder_blender.py` does the steps above from inside Blender. Install it with Edit > Preferences >
Add-ons > (menu) Install from Disk, enable it, and set the Powder repository and the Steep folder in its
preferences. The defaults are `~/Documents/GitHub/powder` and the Lutris path. The 3D view's sidebar (N) gets a
**Powder** tab:

- **Import item**: pick the archive and the item (name or `0x` id), and optionally LOD0 only, textures, or
  extra bones from other items (e.g. `STP_M_Pants_001` for the legs). The item comes in tidied up: stick bones,
  LOD1..3 hidden.
- **Write mod**: give it a mod name, then choose:
  - *Replace the item*: the item's visible LOD objects replace their meshes; hidden LODs stay as they are.
  - *Add selected pieces*: the selected objects are added, and the item keeps its own geometry.
  - *Into*: each LOD by its name, every LOD, or LOD0 only.

  It writes `mods/<name>/client/forge/<Base>/` for **every** datafile that holds the item's meshes (found with
  `forge.py where`), plus a `mod.toml`. Enable the mod with `POWDER_CLIENT_MODS=<name>`.

The tools run in Blender's own Python, which has zstandard and numpy, so this also works in the Steam build of
Blender inside its container. Everything they print goes to the *Powder log* text (Text Editor). Tested on Blender
5.2.2 (headless) and 5.2.3 (Steam):
- import, then *Add selected pieces* with a box named `box@Spine2` into every LOD: 24 vertices at Spine2 = 255
  in the entity's 4 LODs and the Alps copy;
- *Replace* by name, with LOD0 moved up 3 cm: both datafiles' LOD0 is 3.0 cm higher and passes `check`.

## What you get in Blender

- **`Steep_Rig`**, an armature with the bones of every exported skinned mesh. Bones are named as the game names
  them: `Hips`, `Spine2`, `LeftArm`, `LeftHandThumb2`, `RFX_LeftPectoral` and so on. The game stores only
  CRC-32 hashes of the names. 94 names are known, which covers every main body bone. The others (cloth and
  helper bones) are shown as `0x<hash>`. Keep those names as they are, because the name is how a bone is found
  again. The **hierarchy is a guess** (Mixamo's, plus the nearest named bone for helpers), since the skeleton
  resource is not decoded. It only matters for posing in Blender. The **rest pose is real**: each bone sits
  where the mesh's inverse bind matrix puts it.
- **One object per LOD**, `STP_M_Jacket_001_LOD0` .. `_LOD3`. Each is parented to the armature, with an Armature
  modifier and one vertex group per bone that it uses.
- **One material per submesh**, named after Steep's material (`STP_M_Jacket_001_ALL`). With `--textures` it has
  the diffuse map. A material shared by two submeshes of a mesh gets `#sm<k>` on its name.
- **Metres, Z up, the rider facing +Y**, exactly as in the game. The default male rider stands about 1.8 m tall
  in an A-pose.

## Editing: the rules

| you want | do this in Blender |
|---|---|
| reshape an item | edit the LOD object freely: move, add or delete vertices, subdivide, remodel. Any topology works |
| a new part that bends with the body | join it into the LOD object and give it weights: weight-paint it, use a Data Transfer modifier from the original, or Ctrl+P > With Automatic Weights |
| a rigid part (helmet visor, badge, horn) | a separate object either *Parent to Bone* (Ctrl+P > Bone), or named `<anything>@<Bone>` (`visor@Head`) |
| weights to a bone the item does not have | export with `--rig <item that has it>`. The bone is added to the mesh with its rest pose from the file |
| one model for every LOD | delete LOD1..3, rename the object you keep (e.g. `jacket`) and import with `--to LOD`. An object named after no mesh goes into every `--to` mesh, while one still named `..._LOD0` goes only to LOD0 |
| add pieces and keep the original | export only your new objects, import with `--add --to LOD` (`mods/shoulder_pads`) |

- **Keep the material names.** A material picks the submesh. If the mesh has a single submesh, any one material
  goes to it.
- **Don't move bones in Edit Mode.** The rest pose must stay as exported. If the whole armature was moved, the
  importer notices and undoes that. Posing is fine, and the export takes the rest shape anyway, but clear the
  pose (Alt+G/R/S in Pose Mode) before exporting to be safe.
- **UVs** are in the item's texture. To paint a new texture, export it with `texture.py export`, paint, and put
  it back with `texture.py replace` (TEXTURES.md).
- **Limits:** 4 bone influences per vertex (the heaviest four are kept and renormalised to 255), at most 128
  bones per submesh, and at most 65535 vertices per submesh.
- A vertex without weights takes the weights of the nearest original vertex of its submesh, and the import says
  how many did.

Export settings in Blender: format *glTF Binary (.glb)*. The defaults are fine: UVs, normals and skinning are
on, and compression is off (a Draco-compressed file is refused). *Apply Modifiers* is safest off. Animation can be
unticked.

## What the importer does

`import --glb` rebuilds each named mesh from the objects that name it. Several objects for one mesh are merged,
grouped per material.
- **Positions:** a skinned object gives its bind pose. An unskinned object is placed by its world transform.
- **Weights:** skinned objects keep their own weights, matched to bones by name. An object under a bone or named
  `@Bone` is bound 100 % to that bone. `--bone` binds any other unskinned object.
- **Bones:** bones the mesh lacks are appended to its bone list, with the rest pose from the file. The submesh's
  bone map grows to match.
- **Normals and tangent frames:** normals come from the file (or from the faces). Tangent frames come from the UV
  gradients, with the static formats' handedness in the position's w sign.
- **What the file cannot carry** comes from the nearest original vertex of the same submesh: uv1, the skinned
  position's w byte, and the unknown bytes.
- **Unchanged vertices:** a vertex whose position, UV and normal match an original vertex keeps that vertex's
  bytes. An export followed by an import of the untouched file gives back every position, UV, normal and weight.
- **Checks:** the rebuilt mesh must pass `mesh.py check` before it is written.

Supported vertex formats: 0 and 2 (skinned: characters, gear) and 6 and 8 (static: props, rocks, trees). The
heads' format 1, the scarf's format 3 and the rare format 9 are not supported. Exported files from formats 6 and 8
have no armature.

## Tested (2026-10-26, without the game)

- **Jacket round trip** (all 4 LODs). `mesh.py` -> glb -> `mesh.py` gives every position, UV, normal and weight
  back. Through Blender (import, then export untouched) it is the same, except 1-2 extra seam vertices and
  normals within 0.5°.
- **New geometry from Blender:**
  - a cube weighted to `Spine2` comes out as Spine2 = 255 on all 24 of its vertices;
  - a cube weighted to `LeftUpLeg` (a bone the jacket lacks, from `--rig STP_M_Pants_001`) adds that bone, with
    the pants' inverse bind matrix exactly;
  - an object parented to the `Head` bone is bound to Head;
  - an object named `pad@LeftShoulder` is bound to LeftShoulder.
- **Static props:** a tree (format 6, 2 submeshes) and a rock (format 8, no UVs) come back unchanged, also
  through Blender. Rebuilt tangent frames reproduce the stored handedness (2590/2590 w signs).
- **Many meshes:** export and import over every mesh of `DataPC_extra_chr_onboarding`, `DataPC_STP_WhiteRoom` and
  `DataPC` gives 421 meshes back with the same positions, UVs and triangles. The rest are unsupported formats (1,
  3, 9), 7 meshes whose submesh table does not cover their draw ranges (vehicles, parachute) and one
  73,000-vertex helicopter submesh. Over `DataPC_extra_chr` and its `_patch_02` (the gear) 3795 meshes come back
  the same; the only ones left out are 35 heads (format 1) and 3 scarves (format 3).
- **Renders:** the jacket rendered in Blender with the embedded texture shows the panels where they belong.

**Not yet seen in the game:** a rebuilt topology with explicit weights (`mods/shoulder_pads` is that test), and
added bones or a grown bone map (no mod tests them yet). Only same-topology edits are confirmed in-game
(`mods/mesh_edit`).

## Still open

- New items rather than replacements: a new entity, template selector entry, bootstrap item and prefetch entries
  (MESHES.md, "What a fully new model still needs").
- The real skeleton hierarchy, and the names of the other ~600 bones.
- Writing textures from the glTF (today: `texture.py replace` with a PNG).
