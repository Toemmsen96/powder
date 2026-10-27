# master_chief_costume: Master Chief as a new costume

A content mod (native/sdk/CONTENT_MODS.md): **Master Chief** as a costume of its own, next to the game's, with his
name and picture, owned by every player (`grant = true`). Nothing of the game's is replaced. The mod holds only the
model's own files (`assets/`); the game's entries are made from them at each game start against the player's own
install (tools/forge/compose.py, costume_model.py). CONFIRMED in-game 2026-10-27.

**Model:** "Master Chief High Poly" by HarrisonHag1, licensed CC-BY-4.0
(https://sketchfab.com/3d-models/master-chief-high-poly-18f5ee1d13904939971601055102c91b). This work is based on it.
The model is not in git (`models/` is ignored): download it as glTF into `models/master_chief_high_poly/`
(`scene.gltf`, `scene.bin`, `textures/`), then run `mods/master_chief_costume/generate.sh` (the author's step, about
10 s plus Blender for the picture). It writes `assets/`: the fitted models per gender (`chief_M.glb`, `chief_F.glb`),
his maps per Steep material (`textures/`), his picture (`icon.png`) and `model.toml` (the keys in content.toml).

| file | what |
|---|---|
| `make_chief.py` | fits the Sketchfab model onto Steep's skeleton (`--export-assets DIR`) |
| `render_icon.py` | the picture: a front view rendered by Blender (Cycles, CPU) on transparency |
| `content.toml` | the costume: fills costume 016's slots (`STP_M/F_Costume_016`), item COSTUME_MF_POWDER_MASTER_CHIEF |

The first game start with this mod builds Chief (about 9 minutes); later starts use compose's cache.

## How it was made (`make_chief.py`)

1. **Bones.** The model's skeleton (`bip_pelvis`, `bip_spine_0..2`, `bip_upperArm_L`, `f_index.01.L`, ...) maps onto
   Steep's (`Reference`, `Spine`..`Spine2`, `LeftArm`, `LeftHandIndex1`, ...). The knee is Steep's `L_LeftLeg`.
   Joints without a match (bone ends, the root) follow their parent.
2. **Fit, per gender.** Both stand in an A-pose facing -Y. The model is scaled so that head and ankle match. Then
   each bone is turned to point along Steep's bone and moved onto Steep's joint, and every vertex follows its
   weights. The rest pose is the costume's own. The bones it lacks come from the same gender's default gear:
   `Reference` and the forearms. The feet are squashed in height so the soles end on the floor.
3. **Weights.** The model's own weights, renamed to Steep's bones: up to 4 per vertex. A rebuilt submesh gets a
   fresh bone map (47 bones for the whole model).
4. **Back faces.** The model's materials are double-sided, but Steep draws front faces only. 318 triangles wound
   against their normals are turned round, and every triangle gets a back face.
5. **Textures.** Per texture set:
   - **Diffuse:** the base colour.
   - **Normal:** X in R, Y in G (pointing up the image, as glTF's), B = 0. The 4th channel, unknown (it looks like
     ambient occlusion), is set to the default jacket's average.
   - **Specular:** from metalness and roughness.
   - **Mask1:** R = 0. With R = 255 (the jackets' "fabric"), the dark undersuit turned pale cream in the game.
   - Written with `--raw-rows`: the game samples row 0 at v = 0. The first costume versions were written flipped,
     so the suit's quarter showed the visor's texture (pale tan patches) - TEXTURES.md.

