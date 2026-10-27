# Template: a content mod (new costumes, world edits)

Copy this folder to `mods/<your_mod>/`, edit `content.toml` and `mod.toml`, put your files in `assets/`, and switch
the mod on in the launcher's **Mods** page. At the next game start Powder builds every enabled content mod together
against the player's own Steep install (`tools/forge/compose.py`) and the server lists the new items. A mod holds
**only its own files** (models, pictures, edit lists): never files taken out of the game's archives.

| file | what |
|---|---|
| `mod.toml` | `version`, `description` (shown in the launcher; credit models you use here) |
| `content.toml` | the additions: costumes, world edits (commented examples) |
| `assets/` | your files: pictures (`.png`), models (`.glb`), edit lists (`.json`) |

## 1. A copy of a game costume with its own name and picture

The quickest start: `like` names a game costume, `title` and `icon` make it yours. To pick a source:

```
G=~/Games/ubisoft-connect/drive_c/Program\ Files\ \(x86\)/Ubisoft/Ubisoft\ Game\ Launcher/games/Steep
python3 tools/forge/items.py costumes "$G"                 # costume models and the items that use them
python3 tools/forge/items.py show "$G" COSTUME_MF_URBAN_OUTFIT_BIB_131   # one item: no "unlock condition" line
```

Items with an `unlock condition` (Ubisoft Club rewards) are refused: their copies crash the game. The picture goes
over the source's own backdrop (the menu ignores transparency), so a figure on a transparent background looks best;
`mods/master_chief_costume/render_icon.py` renders one from a glTF model with Blender.

## 2. Your own model

A model replaces the slots of a game costume (`costume = "STP_M_Costume_016"`): the costume's materials decide how
the model is split, and each material gets your maps.

1. **Get the costume into Blender** with its armature and material names:
   `python3 tools/forge/mesh.py export "$G/DataPC_extra_chr.forge" STP_M_Costume_016 -o costume_M.glb --textures`
   (and `STP_F_Costume_016` for the female rider). See `tools/forge/BLENDER.md`.
2. **Model over it** in Blender: keep Steep's bones (names and rest pose), weight-paint to them, and give every mesh
   part the name of the costume material it replaces (e.g. `STP_MF_Costume_016_Jacket_ALL`). A part for a material
   you don't need can be one tiny triangle inside the body. Export **glTF 2.0 (.glb)** per gender into `assets/`.
3. **Textures** per material: `diffuse`, `normal`, `specular`, `mask` PNGs laid out on that material's UVs, row 0
   at the top of the UV space (v = 0; `tools/forge/TEXTURES.md`). Missing maps keep the game's.
4. List them in `content.toml` (`model_m`, `model_f`, `textures.<material>.<map>`).

The first game start with a new model builds it (several minutes for a detailed model); later starts use the cache.
`mods/master_chief_costume` is a worked example (its `generate.sh` fits a downloaded model to Steep's skeleton
automatically: `mods/master_chief_costume/make_chief.py --export-assets`).

## 3. World edits

Place, move or remove objects with the in-game **World editor** (overlay, Home, Mods tab), then copy its edit list
into your mod: the server keeps it (`GET https://<server>/_powder/world_editor/edits`, or the last copy in
`data/world_editor/edits.json`). Put it in `assets/`, name it in a `[[world]]` table, and remove the edits from the
World editor so they are not applied twice.

## Testing

```
python3 tools/forge/compose.py --game "$G" --data /tmp/test-data --mods <your_mod> -o /tmp/test-out
```

builds it without the game and reports problems (a path outside the mod, a duplicate name, a refused source). Then
switch it on in the launcher and start the game: the costume is in **Character > Costumes** (granted) or in the
store under **All Costumes** (free).
