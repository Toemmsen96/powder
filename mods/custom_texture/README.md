# custom_texture: a texture we made, in the game

This mod tests whether we can put **new pixels** into Steep. `mods/model_swap` only rearranged the game's own
data. Here the default male rider's jacket gets a diffuse texture that `tools/forge/texture.py` generates and
encodes: an 8x8 checkerboard of magenta and yellow cells with cyan grid lines and **POWDER** written in each
cell. Nothing in it comes from the game except the texture's header: its ids, format, size and mip count.

| replaced entry (keeps its id) | archive (base) | content |
|---|---|---|
| `0x7a5374f6aa` `STP_M_Jacket_001_T01_DiffuseMap` (TextureMap: header + levels 2..11, 512x512 .. 1x1) | `DataPC_extra_chr_onboarding` | the pattern, BC1 |
| `0xeac340d0af` `STP_M_Jacket_001_T01_DiffuseMap_Mip0` (level 0, 2048x2048) | `DataPC_extra_chr_onboarding` | the pattern, BC1 |
| `0xeac340d0b0` `STP_M_Jacket_001_T01_DiffuseMap_Mip1` (level 1, 1024x1024) | `DataPC_extra_chr_onboarding` | the pattern, BC1 |
| `0x7a5374f6aa` `STP_M_Jacket_001_T01_DiffuseMap` (copy) | `DataPC_extra_chr` | the same TextureMap |
| `0x7a5374f6aa` `STP_M_Jacket_001_T01_DiffuseMap` (copy) | `DataPC_STP_Alpes` | the same TextureMap |

The texture is BC1 (format 2), 2048x2048, 12 levels, with the same header as the original. Each payload has the
original's exact length. The normal, specular and mask maps stay the game's own, so the jacket keeps its folds and
seams, but its colour is now the pattern.

## Why these entries

`mods/model_swap/README.md` shows how the menu rider's jacket leads to its textures: save -> loadout item
`JACKET_M_N_001` -> `STP_MF_Template` selector -> material `STP_M_Jacket_001_T01` (`0xeac3434b92`) -> TextureSet
`STP_M_Jacket_001_T01_Set` -> slot 0 (diffuse) = `0x7a5374f6aa`.

- A resource index over the header blocks of all 27 archives shows where these ids live. The TextureMap
  `0x7a5374f6aa` is an entry in three base archives, onboarding, `extra_chr` and `Alpes`, with byte-identical
  payloads. No other datafile embeds it. Its Mip0/Mip1 are entries only in the onboarding archive, and no other
  datafile embeds them either. So these five entries are every copy of the jacket's diffuse pixels.
- None of the ids has a copy in a `_patch_01`/`_02`, so the `_patch_03` that powder-wrap builds for each base
  holds the newest copy.
- The datafiles' dependencies don't change (same resources, same sizes), so the archives keep each id's
  prefetch blob from the stack (all three are the empty blob).

## Use

The entries are built from your install, because their headers are Steep's data:

```
mods/custom_texture/generate.sh [<Steep folder>] [<image.png>]
POWDER_CLIENT_MODS=custom_texture   # for powder-wrap (Linux)
```

With an image, generate.sh uses it instead of the pattern. Give it upright: it is resized to 2048x2048 and stored
bottom-up, as Steep stores textures. To start from the original, export it:
`tools/forge/texture.py export <Steep>/DataPC_extra_chr_onboarding.forge STP_M_Jacket_001_T01_DiffuseMap -o jacket.png`.

**Don't enable `model_swap` together with this mod.** model_swap gives the T01 material the textures of another
colourway, so the jacket would show those textures, and this texture would not be drawn.

## What you should see

In the main menu, rider Freerider M in the Powder account's save wears `JACKET_M_N_001`. That jacket should be
covered in magenta/yellow squares reading POWDER. Depending on how the jacket's UV islands are laid out, the text
can appear rotated or mirrored on parts of the jacket. The colours are what matter. At a distance the game may
draw a smaller mip level (512 and below), where the squares are still clear but the letters blur. Other
colourways of jacket 001 (Salomon, Picture, FWT, ...) have textures of their own and do not change.

## Verified vs assumed

Verified with tools, without running the game:
- the texture format: `tools/forge/TEXTURES.md` (BC1, 2048x2048, 12 levels: 0/1 in the Mips, 2..11 inline);
  the original's levels decode to the blue jacket with orange trim;
- the encoder: a round trip of the original level 0 gives 48.8 dB, Pillow decodes our blocks exactly as
  texture.py does, and the generated levels decode back to the pattern (level 0: 49.3 dB against the source);
- every copy of the texture and its Mips is covered (the resource index above);
- the replaced payloads keep the original's header (TextureMap prefix, Mip header and trailer) and length.
  Only the data differs;
- archives built the way powder-wrap does (`forge.py build <base>_patch_03.forge --like <base>.forge`, in a
  scratch folder) pass `forge.py verify --decompress`, and `texture.py scan` on them finds the Mips matching
  their links.

Assumed (needs the live test):
- the game takes the textures from `_patch_03`, as it did for model_swap's materials (confirmed in-game). Textures
  might be cached or streamed differently from materials;
- the streamed top levels come from the Mip entries in the onboarding `_patch_03`, including when the TextureMap
  is taken from the `extra_chr` or `Alpes` copy;
- the bottom-up row order holds for character textures as it does for UI textures. This only affects which way
  the text faces.

If the jacket looks unchanged, check that powder-wrap's log names the three `_patch_03` archives, and that
model_swap is not enabled. If the jacket shows the pattern only up close or only far away, one of the two
storage places (Mips vs. inline levels) was not picked up.

## Undo

Remove the mod from `POWDER_CLIENT_MODS`, or delete `mods/custom_texture/client/`. The installed game files are
never changed.
