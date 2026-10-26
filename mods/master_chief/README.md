# master_chief: a costume from a downloaded model

This mod turns costume 016 (the camo soldier with gas mask and helmet) into Master Chief, for male and female
riders. A skinned glTF model from Sketchfab is fitted onto Steep's skeleton and replaces the costume's mesh in all
4 LODs, and its textures replace the costume's. A costume is a whole-body item: the game takes the rest of the
outfit off while it is worn, so nothing else is touched.

**Model:** "Master Chief High Poly" by HarrisonHag1, licensed CC-BY-4.0
(https://sketchfab.com/3d-models/master-chief-high-poly-18f5ee1d13904939971601055102c91b). This work is based on it.
The model is not in git (`models/` is ignored). Download it as glTF and unpack it into
`models/master_chief_high_poly/` (`scene.gltf`, `scene.bin`, `textures/`), then run `mods/master_chief/generate.sh`.

**An addition instead of a replacement is not possible yet** (TODO.md). A new costume needs:
- a new item in `Game Bootstrap Settings`, one 146 MB datafile of 17,292 resources, and whatever lists reference
  it (store, unlocks);
- a selector entry in the character template;
- an icon and a localized name;
- ownership from the server.

Most of those formats are not decoded.

## What changes

| what | entries (all in `DataPC_extra_chr`) | how |
|---|---|---|
| the male costume | `STP_M_Costume_016` (entity) with LOD0..3 | `mesh.py import --glb`: armor -> the jacket submesh, undersuit -> the pants submesh, helmet and visor -> the helmet submesh, the glass submesh shrunk to one 1 mm triangle; 63,203 vertices, 56,075 triangles in every LOD |
| the female costume | `STP_F_Costume_016` with LOD0..3 | the same, fitted to the female skeleton |
| the textures | the 12 maps of `STP_MF_Costume_016_{Jacket,Pants,Helmet}_T01_Set` and their `_Mip0`/`_Mip1` | `texture.py replace`: armor and suit at 2048, helmet and visor side by side |

The glass material (`STP_MF_Glass_ALL`) is shared with goggles and other gear, so its textures stay as they are and
only the costume's glass geometry goes. Every costume 016 a rider wears becomes Master Chief.

`mods/shoulder_pads`, `mods/mesh_edit` and `mods/custom_texture` change the jacket, not this costume, so they can
run alongside this mod.

## What you should see

Equip costume 016 under Costumes & Equipments. The rider becomes Master Chief: green armour, black undersuit and the
gold visor, about 1.9 m tall (a little smaller on female riders). He moves with the rider's animations (arms, legs,
fingers), because his own skin weights were renamed to Steep's bones.

## How it was made (`make_chief.py`)

1. **Bones.** The model's skeleton (`bip_pelvis`, `bip_spine_0..2`, `bip_upperArm_L`, `f_index.01.L`, ...) maps onto
   Steep's (`Reference`, `Spine`..`Spine2`, `LeftArm`, `LeftHandIndex1`, ...). The knee is Steep's `L_LeftLeg`.
   Joints without a match (bone ends, the root) follow their parent.
2. **Fit, per gender.** Both stand in an A-pose facing -Y. The model is scaled so that head and ankle match. Then
   each bone is turned to point along Steep's bone and moved onto Steep's joint, and every vertex follows its
   weights. The rest pose is the costume's own. The bones it lacks come from the same gender's default gear:
   `Reference` and the forearms. The feet are squashed in height so the soles end on the floor.
3. **Weights.** The model's own weights, renamed to Steep's bones: up to 4 per vertex. A rebuilt submesh gets a
   fresh bone map (the armor needs 15 bones, the suit 47, the helmet 1: the head).
4. **Back faces.** The model's materials are double-sided, but Steep draws front faces only. 318 triangles wound
   against their normals are turned round, and every triangle gets a back face.
5. **Textures.** Per texture set:
   - **Diffuse:** the base colour.
   - **Normal:** X in R, Y in G (pointing up the image, as glTF's), B = 0. The 4th channel, unknown (it looks like
     ambient occlusion), is set to the default jacket's average.
   - **Specular:** from metalness and roughness.
   - **Mask1:** R = 0. With R = 255 (the jackets' "fabric"), the dark undersuit turned pale cream in the game.

## History

The first version (2026-10-26) replaced the default jacket and shrank the rest of the default male outfit. In the
game it showed with the right shape and armour, which confirmed that bones added to a mesh work. The undersuit came
out cream (Mask1 R = 255), and single-sided shells left see-through spots. Both are fixed here. The costume version
replaces nothing the rider wears every day.

## Verified vs assumed

Verified with tools, without running the game:
- both costumes' 8 meshes pass `mesh.py check`;
- the session archive (`DataPC_extra_chr_patch_03`) builds and passes `forge.py verify --decompress`;
- both fitted models in Blender (5.2.3, through its MCP bridge), textured with the new maps, look right.

Confirmed in the game with the jacket version: the bone mapping, the fit, added bones and the texture layout.

Assumed:
- The costume's LODs take the full model (no reduced LODs yet: some cost at a distance).
- The normal map's 4th channel and the specular map's meaning (G).

## Regenerating and undo

`mods/master_chief/generate.sh [<Steep folder>] [<model folder>]` takes about 6 minutes. Turn the mod off by leaving
it out of `POWDER_CLIENT_MODS`; the installed game files and the save are never changed.
