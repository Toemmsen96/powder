# model_swap: the first asset mod

This mod is a test of whether `.forge` overrides work. It recolours the default male rider's jacket by giving the
jacket's material the textures of another colourway of the **same jacket model**. The mesh, UVs, skeleton and
item stay the same, so the only change is which texture set the jacket shows. Nothing new is imported: every
byte comes from the game's own data.

| replaced entry (keeps its id) | archive (base) | now holds | look |
|---|---|---|---|
| `0xeac3434b92` `STP_M_Jacket_001_T01` | `DataPC_extra_chr_onboarding` | `0x9c6dd17896` `STP_M_Jacket_001_SalomonT02` | blue with orange trim -> orange |
| `0x8af9205b3f` `STP_M_Jacket_001_SalomonT03` | `DataPC_extra_chr` | `0x9c6dd17896` `STP_M_Jacket_001_SalomonT02` | dark navy -> orange |
| `0xeac3434be5` `STP_M_Jacket_004_T01` | `DataPC_extra_chr` | `0x9c6dd7015f` `STP_M_Jacket_004_T02` | orange plaid -> blue plaid |

Each entry is an unpacked datafile under `client/forge/<base>/`. The session tooling builds one archive per base
from them (`forge.py build <base>_patch_03.forge --like <base>.forge <folders>`), so the game is meant to find
the material ids in the newest archive of the stack.

## What you should see

In the main menu, rider **Freerider M** wears item `JACKET_M_N_001` in the Powder account's save. With the mod,
that jacket is orange instead of blue. A Freerider M with the Salomon jacket `JACKET_M_B_SAL03_009` (the older
account's save) also turns orange. The Bone Collector jacket `JACKET_M_N_015`, which Freerider M wears in one
loadout of the Powder account, turns from orange plaid to blue plaid. The jacket's shape does not change. The
zipper pulls keep their old colour, because they are a separate entity with its own texture set.

## How the entries were found

The trace in `powder_asset_trace.log` was no help here. The overlay hooked the file reads only after the game had
opened `DataPC*`, `DataPC_extra*` and `DataPC_extra_chr*`, so none of the character archives' reads are in it
(only the maps' and the videos' reads are). The chain below was followed through the data instead:

1. **Save -> rider and loadout.** Progress save (`STEEP[1]`) section `0x0b00b335` holds the 8 riders (Freerider M,
   Freerider F, Freestyler M/F, Pro Rider, Extreme Rider, Bone Collector, Explorer: bootstrap resources of type
   `0xf5929806`), each with 6 loadouts of item ids. In both accounts on this PC, only Freerider M's loadouts have
   been changed from the defaults. The field right after the section header (property `0xbf4c2013`) is 0 in both
   saves, which presumably selects rider 0, Freerider M. Powder account: `JACKET_M_N_001` in 5 loadouts and
   `JACKET_M_N_015` in 1. Older account: `JACKET_M_B_SAL03_009` in 5 loadouts and a yeti costume in 1.
2. **Item -> tags.** The items are resources of type `0x3856b74b` inside `Game Bootstrap Settings` (`0x800`,
   newest copy in `DataPC_patch_02`). An item carries tag objects: class hash `0xb332698e` followed by a u32.
   `JACKET_M_N_001` has a model tag `0xe0891393` and a texture tag `0xdbedbbbf`.
3. **Tags -> entity and material.** The character template `STP_MF_Template` (`0x7a5370f8b7`, newest in
   `DataPC_extra_chr_patch_02`) has selectors. `STP_M_Jackets` maps the model tag to the mesh entity
   `STP_M_Jacket_001` (`0x8b66b1fe21`). `STP_M_Jacket_001_Textures` maps the texture tag to the material
   `STP_M_Jacket_001_T01` (`0xeac3434b92`) and a zipper entity. Every colourway of jacket 001 (T01, Salomon
   T01-03, Picture T01-04, FWT, ...) uses the same mesh entity and only switches the material. This is why the
   material is the thing to swap.
4. **Material -> textures.** A material datafile has 2 resources: the material (`0x85c817c3`, 1242 bytes) and its
   TextureSet (`0xd70e6670`), which names the Diffuse, Normal, Specular and Mask1 maps. Those maps are entries of
   their own, with `_Mip0`/`_Mip1` entries next to them. Decoding the diffuse maps (BC1 512x512, a 0x8E-byte
   header, read as a guess) showed the colours in the table above.

Why these entries:

- **Each material id lives in exactly one datafile** across all 27 archives (the resource index over every
  header block), so one override covers it. A mesh or a texture would not be that simple. The mesh entity embeds
  its 4 LODs; `STP_M_Jacket_001_LOD0` is also an entry of `DataPC_STP_Alpes`, and the T01 textures are entries in
  three archives (onboarding, `extra_chr`, `Alpes`).
- **Same structure on both sides.** Both are materials of the same layout and length, with the same 2 resources,
  for the same mesh. The engine reads the same kind of object it always reads, so this is the least likely swap
  to crash.
- **No base-archive conflict.** None of the three ids has a copy in a `_patch_01`/`_02`, so `_patch_03` is
  simply the newest copy.

## The swap itself (`forge.py swap`)

The source material is copied into the replaced entry. The entry keeps its id and name, and these parts change:

- **Object ids.** A material holds 7 object ids: its own and 6 nested sub-objects (id+1..+4, plus two ids
  allocated elsewhere, e.g. `0x10edbd23619`). Each is serialized as u64 id + u32 class hash. `swap` maps each
  source id to the id the replaced entry has at the same offset. The result differs from the source material only
  in those 7 ids, and it holds the replaced material's own values there. This was checked byte by byte. The
  mapping is in each `datafile.json` under `swap.ids`.
- **Texture set.** The source's TextureSet comes along unchanged, under its own id, the way texture sets are
  shared by up to 20 datafiles in the shipped data.
- **Prefetch blob.** The archive's `PrefetchingFileInfos` blob for an entry lists the entries it depends on: for a
  material, exactly its 4 textures plus the badge atlas/maps. `swap` stores the source's blob as `prefetch` in
  `datafile.json`, and `build` uses it. Otherwise the game would be told to prefetch the old textures. Shipped
  blobs also point across archives (`extra_chr` -> onboarding 62 times, `Alpes` -> characters 35 times), so a
  blob in the onboarding patch that points at `extra_chr` textures has precedent.

## Verified vs assumed

Verified with tools, without running the game:
- the chain save -> rider -> loadout item -> tags -> template selector -> material -> texture set -> textures
  (from the saves on this PC and the shipped archives);
- each material id occurs in one datafile only, and its override archive is the newest of its stack
  (`forge.py stack` over the game archives plus the built `_patch_03`: `<- ..._patch_03 > ...`);
- the built `_patch_03` archives pass `forge.py verify --decompress`, and their prefetch blobs list the new textures;
- the swapped payloads equal the source material except for the 7 remapped object ids.

Assumed (needs the live test):
- **the game loads `<base>_patch_03.forge`** and lets it win per id, as `_patch_02` wins over `_patch_01`. An
  existence check of `DataPC_STP_Alaska_patch_03.forge` was seen; reads of a patch_03 have not been seen yet;
- the engine does not check the name record's unknown hash (we write 0) and accepts the copied GlobalMetaFile;
- the main menu shows rider 0 with its current loadout (property `0xbf4c2013` = 0 is read as "selected rider";
  that is a guess);
- the selector's material is what is drawn on the jacket. This is near-certain: the Salomon colourways render
  differently in game, yet they share the mesh entity whose embedded default material points at T01;
- the nested object ids: whether the engine cares about them is unknown. Because they are remapped, the swapped
  material shares no object id with its source, so a registry of nested objects (if the engine has one) sees
  no duplicate when both are loaded, e.g. in the gear shop.

If the jacket does not change: check that the session put both `_patch_03` files next to their bases. In
`powder_overlay.log`, asset_trace logs failed opens and existence checks of `.forge` files; with "log reads", it
also shows whether the patch is read. If the game crashes in the menu, the first suspect is the archive, not the
material (the GlobalMetaFile, the zero hash, the prefetch list).

## Undo

Turn the mod off: remove `mods/model_swap/` from the server, or decline it in the launcher. The override exists
only as the generated `_patch_03` archives, which the session tooling adds. The installed game files are never
changed. If a `DataPC_extra_chr_onboarding_patch_03.forge` or `DataPC_extra_chr_patch_03.forge` was ever copied
into the game folder by hand, delete it. The save is untouched: the items stay the same, only their look changes.

## Regenerating

The entries are Steep's own data, so they are not in git (`/mods/*/client/forge/` is ignored): run
`mods/model_swap/generate.sh [<Steep folder>]` once before enabling the mod. It does the following. The payloads are a few KB of the game's own material data. `G`
is the game folder:

```
F=tools/forge/forge.py; M=mods/model_swap/client/forge
python3 $F swap "$G/DataPC_extra_chr_onboarding.forge" 0xeac3434b92 --with "$G/DataPC_extra_chr.forge" 0x9c6dd17896 -o $M/DataPC_extra_chr_onboarding
python3 $F swap "$G/DataPC_extra_chr.forge" 0x8af9205b3f --with "$G/DataPC_extra_chr.forge" 0x9c6dd17896 -o $M/DataPC_extra_chr
python3 $F swap "$G/DataPC_extra_chr.forge" 0xeac3434be5 --with "$G/DataPC_extra_chr.forge" 0x9c6dd7015f -o $M/DataPC_extra_chr
```

To recolour a different jacket, find its material the same way: the rider's loadout item, then its texture tag
in the template's `<model>_Textures` selector. Pick another colourway of the same model, and run `swap`.
