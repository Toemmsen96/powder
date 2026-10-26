# Steep's textures

`texture.py` (next to `forge.py`, which it imports) reads and writes Steep's texture resources:

```
texture.py info    <archive> <entry> [--newest]             # format, size, mips, where the Mip levels live
texture.py info    <unpacked folder | .datafile>
texture.py export  <archive> <entry> -o out.png [--level N | --all-levels] [--raw-rows]
texture.py replace <base archive> <entry> (--image in.png | --pattern checker|stripes [--text POWDER])
                   -o mods/<mod>/client/forge/<base> [--preview p.png] [--raw-rows]
texture.py scan    <archive>...                             # parse every TextureMap/Mip, check every size
```

`replace` takes the entry from the newest archive of the base's stack, as `forge.py swap` does. It resizes the
image to the texture's size and builds the whole mip chain with a 2x2 box filter. It then encodes every level in
the texture's own format and writes unpacked entry folders like `forge.py extract` does: the TextureMap's entry,
plus the entries of its Mip levels where that stack has them. Ids, names, headers and every other resource stay
as they are. Only the pixel data changes, and the sizes stay byte for byte the same, so the entries' dependencies
and prefetch blobs are unchanged (no `prefetch` key is written). Feed the folders to `forge.py build`.

The tool needs numpy. Pillow is optional: it reads non-PNG images, resizes with Lanczos (without Pillow:
nearest), and decodes BC7. PNGs are read and written without Pillow. The encoders are texture.py's own: BC1, BC1
with 1-bit alpha, BC2, BC3 and BC7 (mode 6 only), plus the uncompressed formats.

**C** = confirmed: it holds over all 27 archives (`texture.py scan`, 2026-10-26: 20,725 TextureMap resources,
of which 20,722 parse and 3 have no inline data; 12,965 Mip resources), or on the samples stated. **G** = a
guess, or an observation whose meaning is unknown.

## Resources

| type | what | where |
|---|---|---|
| `0xA2B7E917` TextureMap | header + the lower mip levels | an entry of its own (`*_DiffuseMap`, `*_NormalMap`, `*_Map`, ...), or embedded in another datafile (a `*_MapDesc` carries its TextureMap) |
| `0x1D4B87A3` Mip | one top level (level 0 or 1) of a big texture | an entry of its own, `<texture>_Mip0` / `_Mip1` |
| `0xD70E6670` TextureSet | a material's texture slots | embedded in the material's datafile (`*_T01` + `*_T01_Set`) |

A big texture keeps its top one or two levels in Mip resources, which are separate entries (G: so they can be
streamed). The TextureMap links them. 2048x2048 character textures have 2 links (Mip0 = 2048, Mip1 = 1024), many
1024s have 1 or 2, and some 2048 world/UI textures have none. Mip ids are allocated apart from the texture's id
(`STP_M_Jacket_001_T01_DiffuseMap` is `0x7a5374f6aa`, its Mip0 is `0xeac340d0af`).

A texture's entry can sit in several archives while its Mips sit in only one of them (C, e.g. the
`STP_M_Jacket_001_T01_*` maps: the TextureMap entries are in `DataPC_extra_chr_onboarding`, `DataPC_extra_chr` and
`DataPC_STP_Alpes`, the payloads are identical, and the Mip entries are only in the onboarding archive). To
replace a texture, replace every copy of the TextureMap and the Mip entries wherever they are. `info` on an
archive lists where each Mip lives.

## TextureMap payload

All integers are little-endian. Offsets are for the usual layout; the 3 exceptions are below.

| offset | field | |
|---|---|---|
| 0x00 | u64 resource id, u32 type `0xA2B7E917` | C |
| 0x0C | u8 1 | C value, G meaning |
| 0x0D | u32 width, u32 height, u32 depth | C: they give every data size, including non-powers of two (768x768, 640x360, 44x40) |
| 0x19 | u32 **format** (table below) | C |
| 0x1D | u32 dimension: 1 = 2D; 2 = cube map (the data is 6x a 2D chain); 3 = volume; 0 on one texture (`Noise Gradient Texture`, 256x1) | C sizes, G names |
| 0x21 | u32 1 on diffuse and specular maps, 0 on normal and mask maps | G: sRGB |
| 0x25 | u32 mip count: 0 or 1 means **one level** stored; >= 2 means the **full chain** is stored, down to 1x1. UI icons say 2..5 and still store the full chain (G: a LOD limit) | C on every size |
| 0x29 | u32 usage = the slot in the TextureSet: 0 diffuse, 1 normal, 2 specular, 3 height, 7 mask1, 8 mask2, 11 any other `*_Map` | C slot = usage (52 sets checked); names G, from the entry names |
| 0x2D | u32 group: 1 on character textures, 3 and 5 on UI, also 0 and 2 | G |
| 0x31 | u8: 0, or 1 on 3 textures without inline data (e.g. `PC_STEEP_FPSChart_Map`, a different layout that texture.py refuses) | C |
| 0x32 | u8 0 (1 once) | G |
| 0x33 | u32: 21 on character textures, 50 on UI icons, also 0, 17, 27, 33, 36, 42, 43, 46, 57, 60 | G |
| 0x37 | u32 n links (0, 1, 2), then n x (u8 0, u64 Mip id): link *i* holds level *i* | C |
| +9n | 9 zero bytes | C value, G: an empty link |
| | nested object: u32 class `0x13237FE9`, u32 1, u32 7, u32 width, height, depth, mip count, format, dimension, the 0x21 flag, 3 x u32 0 | C: it always repeats the header |
| | u32 data size, then the data up to the end of the payload | C |

The header is 124 + 9n bytes, so the data starts at 0x7C, 0x85 or 0x8E. The data holds levels n, n+1, ...
largest first, each tightly packed (C). A level is `ceil(w/4) * ceil(h/4) * block bytes` for block formats, or
`w * h * pixel bytes`. Each side halves (rounding down, at least 1) per level. With mip count 0 or 1 the data holds
only level n.

**Rows are stored bottom-up** (C on UI textures: logos, button glyphs, figures and the crash-test dummy all come
out upside down when decoded in storage order. G for the UV-mapped textures, whose orientation the eye cannot
judge). `export` therefore flips the image upright and `replace` flips it back (`--raw-rows` turns this off).
`texture.py`'s round trip was checked: export the jacket's diffuse, `replace --image` with that PNG, and level 0
comes back at 48.8 dB (12 dB if the flip were wrong).

### Mip payload (C: all 12,965 parse)

```
u64 resource id, u32 type 0x1D4B87A3, u8 1, u32 data size, data (one level),
u8 0, u64 TextureMap id, u32 level
```

Every Mip resource agrees with its TextureMap's link on texture id, level and the level's byte size: 12,426 of
12,438 distinct Mip ids. The other 12 are `STP_DEBUG_Grid_*` Mips that no TextureMap links to, and none
disagrees.

## Pixel formats

| code | name | bytes | count | evidence |
|---|---|---|---|---|
| 0 | BGRA8 | 4/px | 112 | **C** size. G order: `Default Normal Texture` holds bytes (255,128,128,255), which is a flat normal (128,128,255) only as B,G,R,A |
| 1 | ? | 4/px | 1 | `Noise Gradient Texture` only (256x1, dimension 0); not supported |
| 2 | **BC1** (DXT1, opaque) | 8/block | 7,851 | **C**: the jacket maps decode to the right images; texture.py's decoder equals Pillow's DXT1 decoder byte for byte; blocks in 3-colour mode never use the transparent index (0 of 18,044 / 80,854 / 26,464 such blocks in the jacket's diffuse/mask/specular Mip0). Diffuse, specular, mask maps |
| 3 | **BC1** with 1-bit alpha | 8/block | 1,026 | **C**: 66 % of blocks in 3-colour mode, alpha 0/255; UI logos/icons decode cleanly (and equal Pillow's) |
| 4 | **BC2** (DXT3) | 16/block | 33 | **C**: UI buttons decode cleanly with explicit 4-bit alpha (equal to Pillow's DXT3) |
| 5 | **BC3** (DXT5) | 16/block | 71 | **C**: FX sprites and UI decode cleanly with interpolated alpha (equal to Pillow's DXT5) |
| 8 | ? (G: BC6H) | 16/block | 189 | cube maps only (`STP_ALA_CM_Sunset_*`), 1 byte/px per face; not decoded |
| 9 | **BC7** | 16/block | 11,378 | **C**: the first byte's lowest set bit (the BC7 mode) spreads over modes 0-7 across 5.7 M blocks with no invalid (zero) first byte; Pillow's BC7 decoder gives clean images. Normal maps (X in R, Y in G, B = 0, another map in A), UI, icons |
| 10 | ? (G: A8) | 1/px | 7 | photo-mode icons; not supported |
| 11 | R8 | 1/px | 49 | **C** size, height maps; G: which channel |
| 14 | ? (G: depth) | 4/px | 1 | `DefaultDepthTexture` |
| 19 | ? (G: RGBA16F) | 8/px | 4 | `STP_DEMO_Cine_*` |

The sRGB flag (0x21) is set independently of the format: BC1 serves diffuse (sRGB 1) as well as masks (0).
`replace` keeps the header, so it keeps the flag too.

### Encoder round trips

Each original level was decoded, encoded with texture.py and decoded again. PSNR against the first decode:

| texture | format | PSNR |
|---|---|---|
| `STP_M_Jacket_001_T01_DiffuseMap` level 0 (2048, Mip0) | BC1 | 48.8 dB (Pillow's own BC1 encoder: 41.3 dB) |
| same, level 2 (512, inline) | BC1 | 48.2 dB (Pillow: 40.5 dB) |
| `..._SpecularMap` / `..._Mask1Map` level 1 | BC1 | 50.2 / 47.0 dB |
| `..._NormalMap` level 2 | BC7 (mode 6 only) | 35.4 dB RGB, 31.9 dB alpha |
| `Steep-UI-Brands-northFace_Map` | BC1 + alpha | 43.2 dB, alpha exact |
| `STEEP-boutons-01-RS_Map` | BC2 | 53.9 dB, alpha exact |
| `xPickSmokeGrey_10x2_DiffuseMap` | BC3 | 43.8 dB, alpha 78.9 dB |
| `Default Normal Texture`, a height map | BGRA8, R8 | exact |

Pillow decodes texture.py's encoded BC1/BC2/BC3 data to exactly what texture.py's own decoder gives, so the
blocks are standard. BC7 is decoded by Pillow only (texture.py has no BC7 decoder of its own), and it reads the
encoder's blocks back at the PSNR above. The BC7 encoder uses one mode with no partitions: its blocks are valid,
but the quality is not the best.

## TextureSet (`0xD70E6670`, 202 bytes) (C on 52 sets)

```
u64 id, u32 type, u8 1,
18 slots x (u8 tag, u8, u64 id): tag 1 + u8 1 + a TextureMap id, or tag 3 + 0 + 0 for an empty slot,
u8 0, u64 id (G: an editor object; mostly the set's own id - 1, which is in no archive)
```

The slot index equals the texture's usage field: 0 diffuse, 1 normal, 2 specular, 7 mask1 on every jacket
material. A material's datafile embeds its TextureSet, and many materials share one (see `mods/model_swap`).
The material refers to the set, so a material swap changes which textures are drawn, while replacing a texture
changes its pixels for every material that uses it.

## Open questions

- Formats 1, 8, 10, 14 and 19 (rare): which DXGI formats exactly, and the face order of cube maps.
- The fields at 0x0C, 0x2D, 0x32, 0x33 and the 9 zero bytes after the links; the layout with 0x31 = 1.
- Whether the engine needs the Mip entries next to a TextureMap copy, or finds them in any loaded archive (the
  shipped data relies on the latter: two of the three jacket copies have no Mips beside them).
- Whether the menu draws the TextureMap's inline levels or streams Mip0/Mip1, and from which archive copy.
  `mods/custom_texture` replaces all of them, so either way the result should show.
