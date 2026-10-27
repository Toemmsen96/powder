# master_chief: a costume from a downloaded model

This mod turns the **penguin costume** (`COSTUME_MF_PENGUIN_91`, its own full-body model `STP_M/F_Costume_005`) into
Master Chief, for male and female riders, and gives the costume a **Chief picture** in the costume list (the menu
icon `COSTUME_PENGUIN_MF_Map`, rendered from the model by `render_icon.py` when Blender is found). A skinned glTF
model from Sketchfab is fitted onto Steep's skeleton. The penguin has a model of its own, so no other gear changes.
`CHIEF_TARGET=military` (the military costume's coat; the rest of that set hidden, shared gear included) or
`CHIEF_TARGET=urban_japan` (costume 016, not in the store) put him elsewhere.

Why not a new costume item, or the unicorn: a new item is loaded by the game but not listed (it lacks a store GUID,
see tools/forge/ITEMS.md; tied to an unused item-list entry's name it still did not show, 2026-10-27). The unicorn
(`COSTUME_MF_UNI_012`) is a set of regular, shared models (`STP_M_Jacket_008`, `STP_M_Pants_007`, ...), so Chief
there would change other jackets and pants too.

**Model:** "Master Chief High Poly" by HarrisonHag1, licensed CC-BY-4.0
(https://sketchfab.com/3d-models/master-chief-high-poly-18f5ee1d13904939971601055102c91b). This work is based on it.
The model is not in git (`models/` is ignored). Download it as glTF and unpack it into
`models/master_chief_high_poly/` (`scene.gltf`, `scene.bin`, `textures/`), then run `mods/master_chief/generate.sh`.

**An addition instead of a replacement** is the next step: `tools/forge/items.py add-costume` (mods/custom_costume_test)
adds a new costume, and the game loads it into its costume list (seen in its memory, 2026-10-27). How to make it
show up and be owned (it has no store GUID) is still open.

## Which costume

A costume's model is chosen by the character template (`STP_MF_Template`): its `STP_M_Clothing` / `STP_F_Clothing`
selectors map a tag of the item to the costume's own selector, which names the entity. Read from the data
(2026-10-27; * = in the game's store):

| model (entity) | items |
|---|---|
| `STP_M/F_Costume_001` | Halloween 2018*, sumo jackets* |
| `_002` | banana* |
| `_003` | T-Rex* (5 colours) |
| `_005` | **penguin*** (`JACKET_MF_C_Penguin_228`, costume `COSTUME_MF_PENGUIN_91`) |
| `_010` | spandex* (5 colours) |
| `_011`, `_013` | Olympic jackets, mascot |
| `_014` | urban outfit bib* |
| `_016` | urban outfit Japan (`COSTUME_MF_URBAN_OUTFIT_JAPAN_139..141`), not in the store |
| `_017` | 90s* |

The first costume version replaced 016 on the guess that it was the military costume. It is not: the military
costume is a bundle of regular gear: coat `JACKET_MF_C_Military_233/234` -> `STP_M_Jacket_013` / `STP_F_Jacket_010`,
pants -> `STP_M_Pants_011` / `STP_F_Pants_009`, helmet with the skull mask -> `STP_M/F_HelmetFun_048`, harness ->
`STP_M/F_Backpack_002` + belts, gloves -> the shared `STP_MF_Gloves_001` (`items.py show <item>`). 016's items are not
in the store, so nobody could equip it. The penguin version was seen in the game (with flipped textures, below).

## What changes

| what | entries (all in `DataPC_extra_chr`) | how |
|---|---|---|
| the coat | `STP_M_Jacket_013`, `STP_F_Jacket_010` (entities, LOD0..3) | `mesh.py import --glb`: the whole model into the coat's one submesh; 63,200 vertices, 56,074 triangles in every LOD |
| the textures | the coat's two colourways (`STP_MF_Jacket_013_T01`, `_T02`: 2 texture sets x 4 maps, with `_Mip0`/`_Mip1`) | `texture.py replace --raw-rows`: armour, suit, helmet and visor packed 2x2. The coat's embedded material holds the shared placeholder set, which stays |
| the rest of the set (military only) | pants, helmet, harness and belts (M and F, 12 entities) | `mesh.py edit --scale 0.001` |
| the menu picture | the costume's icon texture (`DataPC_extra_2`, 256 x 256 BC7) | `render_icon.py` (Blender, Cycles on the CPU, a front view on a transparent background), blended onto the icon's own backdrop with the penguin painted out (the menu draws an icon's colours and ignores its alpha: a black backdrop showed as a dark gradient, 2026-10-27), `texture.py replace` |
| the bundle's extras (penguin) | backpack harness, gloves, ski and snowboard boots: shared models, but colourways only the penguin's items use (`..._FunT25`, `..._T19`) | diffuse maps tinted Chief green (`recolour`) |

Every item that uses these models becomes Chief (or vanishes): the coat model in other colourways, the pants,
helmet and harness on their own.

`mods/shoulder_pads`, `mods/mesh_edit` and `mods/custom_texture` change the jacket, not this costume, so they can
run alongside this mod.

## What you should see

With Powder's store (no game-data pack, `data/store_items.json` from `tools/trainer/storemap.py`), every costume is a
free offer. Take the **penguin** costume (its tile now shows Chief) under Costumes & Equipments and equip it. The rider becomes Master Chief: green
armour, black undersuit and the gold visor, about 1.9 m tall (a little smaller on female riders). He moves with the
rider's animations (arms, legs, fingers), because his own skin weights were renamed to Steep's bones.

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

## History

The first version (2026-10-26) replaced the default jacket and shrank the rest of the default male outfit. In the
game it showed with the right shape and armour, which confirmed that bones added to a mesh work. The undersuit came
out cream (Mask1 R = 255), and single-sided shells left see-through spots. Both are fixed here. The costume version
replaces nothing the rider wears every day.

## Verified vs assumed

Verified with tools, without running the game:
- both penguin costumes' 8 meshes pass `mesh.py check`;
- the session archive (`DataPC_extra_chr_patch_03`) builds and passes `forge.py verify --decompress`;
- the fitted models looked right in Blender (5.2.3, through its MCP bridge) for the 016 version; the fit and
  textures are the same for the penguin.

Confirmed in the game with the jacket version: the bone mapping, the fit, added bones and the texture layout.
Confirmed 2026-10-27 with the penguin version: Chief in the costume list with his picture (then still on a dark
backdrop, and with the penguin's yellow boots, gloves and harness; both fixed since, not yet seen in-game).

Assumed:
- The costume's LODs take the full model (no reduced LODs yet: some cost at a distance).
- The normal map's 4th channel and the specular map's meaning (G).

## Regenerating and undo

`mods/master_chief/generate.sh [<Steep folder>] [<model folder>]` takes about 2 minutes (`--target urban_japan` for the 016 version). Turn the mod off by leaving
it out of `POWDER_CLIENT_MODS`; the installed game files and the save are never changed.
