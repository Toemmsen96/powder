# master_chief: a whole character from a downloaded model

This mod puts Master Chief on the default male rider (Freerider M). A skinned glTF model from Sketchfab is fitted
onto Steep's skeleton and takes the place of the default jacket, `STP_M_Jacket_001`, in all 4 LODs. Its textures
replace the jacket's texture set, and the rest of the default male outfit is shrunk away so nothing pokes through.

**Model:** "Master Chief High Poly" by HarrisonHag1, licensed CC-BY-4.0
(https://sketchfab.com/3d-models/master-chief-high-poly-18f5ee1d13904939971601055102c91b). This work is based on it.
The model is not in git (`models/` is ignored). Download it as glTF and unpack it into
`models/master_chief_high_poly/` (`scene.gltf`, `scene.bin`, `textures/`), then run `mods/master_chief/generate.sh`.

## What changes

| what | entries | how |
|---|---|---|
| the body | `STP_M_Jacket_001` (entity, `DataPC_extra_chr_onboarding`) with LOD0..3, and `STP_M_Jacket_001_LOD0` (`DataPC_STP_Alpes`) | `mesh.py import --glb`: 31,600 vertices, 28,037 triangles in every LOD; 38 bones added (legs, forearms, fingers) |
| the textures | `STP_M_Jacket_001_T01_{Diffuse,Normal,Specular,Mask1}Map` in `DataPC_extra_chr_onboarding`, `DataPC_extra_chr`, `DataPC_STP_Alpes`, and their `_Mip0`/`_Mip1` | `texture.py replace`: the model's 4 materials packed 2x2 |
| the rest of the outfit | 43 datafiles: head (+ beard, teeth, tongue), hair, hat, helmets, goggles, gloves, pants, boots, scarves, backpacks, harness, the jacket's zippers, jacket 002 | `mesh.py edit --scale 0.001`: each shrunk to a point |

These items are shared: every male rider who wears one of them loses it too, and anyone wearing the default jacket
becomes Master Chief. `mods/mesh_edit`, `mods/shoulder_pads` and `mods/custom_texture` replace some of the same
entries, so enable only this one. With two of them, building the session's `_patch_03` fails on the duplicate ids,
and powder-wrap logs that and leaves the archive out.

## What you should see

In the main menu, Freerider M is Master Chief: green armour, the gold visor, about 1.9 m tall. He should move with
the rider's animations (arms, legs, fingers), because his own skin weights were renamed to Steep's bones.

If something goes wrong:
- *Still the old outfit.* Check powder-wrap's log for "asset mods: ..." with the three archives.
- *Chief shows but other clothes poke through.* Freerider M wears an item that is not in the list above; tell
  which, and it is added to `HIDE` in `make_chief.py`.
- *Chief is invisible, stretched or exploded.* The added bones (legs, fingers) are the suspect. This is the first
  mod that adds bones to a mesh.
- *A crash.* Turn the mod off and report it.

## How it was made (`make_chief.py`)

1. **Bones.** The model's skeleton (`bip_pelvis`, `bip_spine_0..2`, `bip_upperArm_L`, `f_index.01.L`, ...) maps onto
   Steep's (`Reference`, `Spine`..`Spine2`, `LeftArm`, `LeftHandIndex1`, ...). The knee is Steep's `L_LeftLeg`.
   Joints without a match (bone ends, the root) follow their parent.
2. **Fit.** Both stand in an A-pose facing -Y. The model is scaled from its own units (0.000278 m each) so that
   head and ankle match. Then each bone is turned to point along Steep's bone and moved onto Steep's joint. The
   rest poses come from the default jacket, gloves, pants and boots, and every vertex follows its weights. The feet
   are squashed in height so the soles end on the floor.
3. **Weights.** The model's own weights, renamed to Steep's bones: up to 4 per vertex.
4. **Textures.** One 2048 atlas per map. Armour top left, suit top right, helmet bottom left, visor bottom right;
   1024 each, with UVs moved into their quarter.
   - **Diffuse:** the base colour.
   - **Normal:** Steep keeps X in R, Y in G (pointing up the image, as glTF's) and B = 0. The 4th channel, unknown
     (it looks like ambient occlusion), is set to the jacket's average.
   - **Specular:** from metalness and roughness.
   - **Mask1:** R is white on fabric and black on hard parts, as the jacket's.
5. **Out.** A glb with the fitted body skinned to Steep's bones. `mesh.py import --glb ... --to LOD --ignore-names`
   puts it into every LOD of every datafile that holds a jacket LOD (`forge.py where`). It adds the bones the
   jacket lacks with their rest pose (the jacket's bone map grows from 62 to 100 of 128).

Seen in Blender (5.2.3, through its MCP bridge), with the fitted glb and the atlas: the model stands on the floor,
textured, with Steep's joints inside shoulders, elbows, hips, knees and ankles.

## Verified vs assumed

Verified with tools, without running the game:
- every rebuilt and shrunk mesh (139) passes `mesh.py check`;
- the three session archives built from the entries pass `forge.py verify --decompress`;
- the diffuse's streamed top level (`_Mip0`, 2048) decodes to the atlas, the right way up.

Assumed (this mod is the test):
- **The engine accepts bones added to a mesh**, with their own inverse bind matrices.
- The normal map's green points up the image (as glTF's) and its 4th channel is not critical. A wrong guess shows
  as inverted bumps or odd shading, not as a broken model.
- The mask and specular maps' meanings (G).
- Every LOD carries the full 31,600 vertices. There is no reduced LOD yet, which costs some performance at a
  distance.

## Regenerating and undo

`mods/master_chief/generate.sh [<Steep folder>] [<model folder>]` takes about 3 minutes. Turn the mod off by
leaving it out of `POWDER_CLIENT_MODS`; the installed game files and the save are never changed.
