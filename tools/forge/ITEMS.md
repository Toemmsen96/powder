# Steep's gear items, the character template, and adding a new costume

`items.py` reads the gear items in `Game Bootstrap Settings` and the selectors of the character templates, and it
adds a **new** costume: an item with a new id that the game lists next to its own, drawn by a new entity with new
meshes, materials and textures. Nothing of the game's is replaced. It uses `forge.py` from the same folder (Python
3.10+, zstd). Archives are only read; `_patch_NN` past `_patch_02` (asset mods' session archives) are left out.

```
items.py list      <Steep> [-g NAME] [--category 25]      # items: id, name, category, price, flags
items.py show      <Steep> COSTUME_MF_URBAN_OUTFIT_JAPAN_139  # fields, tags, links, the template branches its tags pick
items.py costumes  <Steep>                                # STP_M/F_Clothing: tag -> costume selector -> entity, items
items.py add-costume <Steep> --like ITEM | --costume STP_M_Costume_NNN --name NEW_NAME -o mods/<mod>/client/forge
                     [--source-dir <another mod's client/forge/DataPC_extra_chr>]... [--keep-price] [--label L]
items.py check     <Steep> mods/<mod>/client/forge/NEW_NAME.json [--source-dir ...]
```

**C** = confirmed on the shipped data (every item / selector, or the samples stated). **G** = a guess, or a field
whose meaning is unknown. Nothing here has been confirmed in the game yet: the game has never loaded an item we
added (see "What the game must accept" at the end).

## The chain: item -> tags -> selector -> entity -> materials -> textures

```
Game Bootstrap Settings (0x800, DataPC stack)          STP_MF_Template / STP_F_Template (DataPC_extra_chr)
  GEAR_COSTUMES (0x5F0A43A7) --lists--> item                + STP_M/F_NPC_Template (DataPC): the same selectors
  item (0x3856B74B): tags [0x77da12eb, 0xa9af2d6d, ...]
                              |                         STP_M_Clothing (0x22ECBE63)
                              +--- tag 0x77da12eb ----->   branch tag 0x77da12eb -> STP_M_Costume_016, STP_MF_Accessories
                                                        STP_M_Costume_016: one branch, children by slot:
                                                           1 entity 0x158bc190b2e   (DataPC_extra_chr entry)
                                                           2 STP_MF_Costume_016_Textures
                              +--- tag 0xa9af2d6d ----->   STP_MF_Costume_016_Textures: branch tag 0xa9af2d6d (T01)
                                                              -> materials Helmet_T01, Jacket_T01, Pants_T01, Glass_ALL
                                                           3 STP_MF_BrandManager_Jacket, 4 STP_SkelAddon_M_UrbanCostume,
                                                           5 STP_MF_Gloves
material entry (0x85C817C3) -> its TextureSet (0xD70E6670) -> 4 TextureMaps -> their Mip0/Mip1 entries
```

- **C** `costumes` lists every costume branch. Costume selectors 001..017 are drawn by two kinds of item:
  - *self-contained* costume items (category 25) whose own tags hold the model tag: 010 (`COSTUME_MF_C_SPANDEX_*`),
    014 (`COSTUME_MF_URBAN_OUTFIT_BIB_*`), **016 (`COSTUME_MF_URBAN_OUTFIT_JAPAN_139/140/141`)**, 017
    (`COSTUME_MF_C_90sT0*`), 001 (`COSTUMES_MF_N_HAL2018_151`);
  - *bundles*: the costume item (e.g. `COSTUME_MF_PENGUIN_91`, `COSTUME_MF_BANANA_39`) carries no model tag; it links
    its part items (jacket, pants, gloves, ...), and the **jacket** item carries the model tag: penguin -> 005 via
    `JACKET_MF_C_Penguin_228`, banana -> 002, T-rex -> 003, sumo -> 001, Olympics -> 011, mascot -> 013.
  - Other "costumes" are plain gear sets: `COSTUME_M_MILITARY_GREY_94` links `JACKET_MF_C_Military_233` etc., which
    use the ordinary jacket/pants selectors (jacket 013, pants 009/011). **Costume 016 is not the military costume.**
- **C** Costume 016 is drawn only by `COSTUME_MF_URBAN_OUTFIT_JAPAN_139/140/141` (T01/T02/T03: their texture tags
  0xa9af2d6d / 0x30a67cd7 / 0x47a14c41 pick the colourway). None of the three has a store GUID in the running game's
  catalogue (`build/store_items.json` from `tools/trainer/storemap.py`), so they are probably not obtainable in the
  game, which matters for `mods/master_chief` (it replaced costume 016).
- **C** For jackets the item's model tag picks the mesh entity in `STP_M_Jackets`, and its texture tag picks the
  material in `STP_M_Jacket_NNN_Textures` (mods/model_swap/README.md).

## Item (resource type 0x3856B74B, 2,438 in Game Bootstrap Settings)

Little-endian; nested objects are `u64 id, u32 class, body`; a link is `u8 0, u64 id`. `parse_item` decodes 2,314
items (95%) with this layout (C); the other 124 (Olympics / freestyle / "OG" items) have lists inside the item block
(links to other items) and are not decoded (G).

| offset | field | |
|---|---|---|
| 0x00 | u64 id, u32 class 0x3856B74B | C |
| 0x0C | 7 bytes `01 01 00 ff ff ff ff` (`01 00 00 ...` on NONE_ITEM_TOOL) | G |
| 0x13 | LocString object: u64 id, class 0x81A7045D, u32 **string id** (the localized name), u32 n, n UTF-16 chars + NUL (a debug name, e.g. "Military Jacket 01"; empty on most) | C layout, G meaning |
| | u8 3, u32 n, n links: **unlock conditions** (0x79BECE6D, e.g. "Kevin Rolland Scarf Unlock Condition") | C |
| | item block: u64 id, class 0x72873A8E, 96 bytes | C size |
| block+0x08 | u8: 1, except 0 on the OG/Olympics items and the `*_NONE_EMPTY` items | G: shown in the shop |
| block+0x3E | u32 **price** (0 on 1,641 items; 12000, 2200, 56000, ...; 48000 on the urban outfit) | G: credits |
| block+0x4A | u8 **default**: 1 on 109 items: the stock riders' starter gear (`JACKET_M_N_001`, `GOGGLES_MF_N_001`, ...) and every `*_NONE` item | G: owned from the start |
| block+0x4B | u8 1 on the 9 "none" items (`COSTUME_NONE`, `HELMET_NONE`, ...) | G: the empty choice |
| | 5 icon objects: u64 id, class 0x38A922C1, u8 kind (3; 1 seen twice), link A, link B. The first one's B is the **icon TextureMap** (e.g. `STP_ICON_COSTUME_MF_URBAN_OUTFIT_JAPAN_139_00006_Map` in DataPC_extra_2) | C layout, G meaning |
| | 3 x u8 | G |
| | 7 **tag lists**: u32 n, n x (u64 0, class 0xB332698E, u32 tag). List 0 holds female tags, 1 male, 2 both: 142 F items use only list 0, 123 M items only list 1, 1,018 MF items only list 2. Lists 3..6 are rarely used | C layout, G meaning of 0..2 |
| | u32 n, n links: variants (the colours of a harness, ...) | C |
| | u32 **category** = the gear slot: 4 helmet, 5 scarf, 7 hat, 8 goggles, 9 jacket, 10 gloves, 11 pants, 15 ski, 16 snowboard, 17 backpack, 18 wingsuit, 25 costume, 28 rocket wingsuit, ... (from the names) | C |
| | tail: part links (a costume's "none" jacket/pants/helmet; a bundle's parts; a part's costume), and two objects of class 0x7E42F87F with ids allocated in the last patch (0x1bd3...) | G |

- Store GUIDs are in no archive (C): owning an item = its Ubisoft GUID in the inventory; the running game holds the
  catalogue (storemap.py). 312 items have no GUID there: the `*_NONE` items, the Olympics items, and a few others,
  including the urban outfit Japan.
- **G (to test): a new item has no GUID.** `add-costume` therefore sets the price to 0 and the default flag
  (block+0x4A), the two fields the stock riders' gear has. Whether the game then treats it as owned, or lists it at
  all, is the first thing to see in the game (`--keep-price` keeps the source's values).

## Lists that reference items

- **GEAR_* lists** (type 0x5F0A43A7, 30 in the bootstrap, e.g. GEAR_COSTUMES 0xecf0f1e2b1): u64 id, class, u8 1, a
  tag record (u64 0, class 0xB332698E, u32 0), u32 category, then two lists of `u8 kind, u8 0, u64 id` (kind 1 = a
  link, 3 = none): the slot's "none" item (`COSTUME_NONE`, or none), then **all items of the slot**, which ends the
  payload (C on 28; BOOSTERS and TICKETS have more lists). GEAR_COSTUMES lists 151 items. `add-costume` appends the
  new item to the list that holds the source item.
- **PACK_*** (type 0xB093CC76, e.g. "PACK_WINTERFEST DLC CONDITION_DLC_PACK_4042785157" -> the banana costume): DLC
  and reward packs. A new item is in none (G: not needed to show it).
- The bootstrap's root (0x800) links only top-level objects (Game Settings, Universe, seasons, ...), not items (C):
  items are reached through the GEAR lists, packs and other items.

## Tag registry (0xBD6, type 0xFFC5A970)

u64 id, class, u8 1, u32 n (207 categories), then per category `u8 0, u64 id, class 0x0196529F, u8 1, u32 m` and m
entries `u64 0, class 0xA31AA51D, u32 tag, u32 group, u8 0`. Some entries end in `01` and hold a sub-category or a
nested entry (not fully decoded). The 8 costume model tags (0x3fef8bc8, ..., 0x77da12eb, 0x00dd227d) form one
category (0x144a4d2bdeb, group 0x39cda93a) that is a sub-category of tag 0x9941e48e; costume 016's three texture
tags another (C). `add-costume` registers its new model tag in the source tag's category (G: whether the engine
needs it).

## Selector (type 0x22ECBE63, 416 in STP_MF_Template)

| part | contents | |
|---|---|---|
| head | u64 id, class 0x22ECBE63, u8 1, u32 n slots, n slot objects (u64 id, class 0x36839608, ..., the slot's index and the class it takes: 0xEC6AC357 a mesh entity, 0x22ECBE63 a selector, 0x85C817C3 a material, 0x24AECB7C a skeleton addon; a material slot also names a default material) | C class, G sizes |
| branches | u32 n, n x: u64 id, class 0x348B28D6, f32 weight (1.0 on the default branch, else 0), u8 1, u64 owner (the selector), tag record (u64 0, class 0xB332698E, u32 **tag**; 0 = the default branch), tag list object (u64 id, class 0x11BD5345, u32 m, m tag records), u32 k children | C on 398 of 416 |
| child | u32 slot, u32 class, u16 0, u16 kind, u32 0, then kind 0x1C (inside the template): `01 00` or `03 00` + u64 id; kind 0x12 (an entry elsewhere: entity, material): `00` + u64 id | C |
| tail | u8 0, object 0xF5BD7B8A, object 0x11BD5345, ..., u64 own id, ..., then **u32 n, n links: every selector below this one**, depth first, each once; it ends the payload | C on 416 |

- The template object (0x971A842E) and the ancestors of a Clothing selector (STP_MF_Parts, STP_MF_Activities,
  STP_MF_Activities_Air, STP_M_Activities, STP_M/F_Clothing) hold such depth-first lists too: `costumes` order is
  `... STP_M_Costume_016, STP_MF_Costume_016_Textures, STP_M_Costume_017, STP_MF_Costume_017_Textures, <F part> ...`
  (C). `add-costume` inserts its new selectors right after the Clothing selector's span in every list that holds
  the source costume selector (G: whether the order matters).
- **The same selector is embedded in several templates, byte for byte** (C): STP_M_Clothing, STP_M_Costume_016,
  STP_MF_Parts sit in STP_MF_Template (DataPC_extra_chr) and STP_M_NPC_Template (DataPC); STP_F_Clothing,
  STP_F_Costume_016, STP_MF_Costume_016_Textures also in STP_F_Template and STP_F_NPC_Template. `add-costume`
  patches all four the same way, so no two copies of an id differ. Lists also name selectors their datafile lacks
  (STP_MF_Parts in the M NPC template), so they are patched by content, not by what the datafile holds.

## Entity, material, textures

- Costume entity (0x51DC6B80, 207 bytes) as MESHES.md describes; its LOD objects (class 0xA360319A) have ids of their
  own (C). The entity datafile also carries the default `_ALL` materials and their T01 TextureSets inline; the
  texture selector's material slots name those `_ALL` materials as defaults (C).
- A mesh names its materials near its end (submesh material links; C on costume 016's 4 LODs).
- Material entry: material + TextureSet; the material has 4 nested objects with ids id+1..id+4 (class 0x7D08460D)
  (C on costume 016's). TextureSet: 18 slots of TextureMap ids (TEXTURES.md). TextureMap: links its Mips; a Mip
  links back (TEXTURES.md).
- Prefetch blobs: an entity's lists its textures; a material's its 4 maps plus the badge atlas (C). `add-costume`
  copies each source entry's blob with the ids remapped.

## Ids

- Shipped ids are 35..41-bit numbers; the highest resource id in all 27 archives is 0x1bd52dc0def (C, 375,442 ids).
  Objects nested in a resource have ids of their own, allocated from the same counter (mostly the resource's id + 1
  .. + k), which are in no archive's header.
- **New ids**: `add-costume` takes a block of 4,096 at `0x1F000000000 + (crc32(name) & 0xFFFFF) * 0x1000`, above every
  shipped id and the same for the same name (so a regenerated mod keeps its ids; a save that holds the item stays
  valid). It skips any id that is a resource anywhere.
- **New tag**: crc32("POWDER:" + name), stepped past any tag an item or the registry uses.
- What gets a new id: the item and its nested objects (its LocString, block, icon and 0x7E42F87F objects); the
  costume selectors (M, F) and their texture selectors with their nested objects (slots, branches, lists); the new
  Clothing branches; the entities, their LOD meshes and the costume's own materials, TextureSets, TextureMaps and
  Mips (everything named `*_Costume_NNN*`), with the nested objects of entities and materials. Shared things keep
  their ids and are referenced as they are: STP_MF_Glass_ALL, STP_MF_CHR_PLACEHOLDER_Set, the badge atlases, the
  skeleton addon, the gloves/brand/accessories selectors, the icon, the localized name.
- Nested ids are recognised by class (`NESTED_CLASSES`: counted over every item and selector and 400 character
  entries). A looser rule (any u64 in the id range followed by four nonzero bytes) took bool arrays in materials
  (`01 01 01 00 01 01 00 00`) for ids; `check` would now catch that (every changed byte must be a remapped id).
- Not remapped (G): ids inside mesh bodies other than the mesh's own id and its material links; a TextureSet's last
  u64 (TEXTURES.md: an editor object, id - 1, in no archive) keeps the source's value.

## What add-costume writes

`-o mods/<mod>/client/forge` gets the folders powder-wrap builds into `_patch_03` archives:

| folder | entries |
|---|---|
| `DataPC/` | `Game Bootstrap Settings` (the new item after its source, the GEAR list, the tag registry), `STP_M_NPC_Template`, `STP_F_NPC_Template` as packed `.datafile`s (146 MB uncompressed; packed once here, so the session build only copies them) |
| `DataPC_extra_chr/` | `STP_MF_Template`, `STP_F_Template` (`.datafile`); one unpacked folder per cloned entry (entities, materials, textures), each with its prefetch blob and a `clone` note in `datafile.json` |
| `<name>.json` | the report: tags, ids (old -> new), selectors, entities |

`check` re-reads all of it against the game: one resource more in the bootstrap and only the GEAR list and the
registry changed; the new branch in both Clothing selectors; every list that held the source selector holds the new
one; copies of a selector that were the same in every template still are; in each cloned entry every changed byte is
a remapped id and shared resources are untouched; no reference to an id that exists nowhere. The archives built from
the output pass `forge.py verify --decompress`.

Tested (2026-10-27, no game run): costumes 016 (via the urban outfit item), 001, 003, 005, 010, 017 (via
`--costume`, which picks a costume item if one carries the tag, else the bundle's jacket: costume 005 gives a new
**jacket** item in GEAR_JACKETS that draws the penguin), and 016 from mods/master_chief's generated entries
(`--source-dir`): `check` 0 problems each; for 016 a full scan of every cloned payload found no source id left;
both `_patch_03` archives (DataPC 118 MB, DataPC_extra_chr 95 MB) built and verified, also together with
master_chief's entries.

## What the game must accept (open, in the order to test)

1. **DataPC_patch_03.** CONFIRMED 2026-10-27: the game loads it; the new item sits in the game's in-memory costume
   list right after the three URBAN_OUTFIT_JAPAN items it was cloned from.
2. **The item in the menu.** Not seen in the menu or the store (2026-10-27, mods/custom_costume_test): the menu
   follows the store, and a new item has no GUID. How the game ties GUIDs to items (read from the running game,
   `tools/trainer/storemap.py --item-list`): the **space's item list** (UbiServices `GET
   /v1/spaces/<space>/items/all`), 3,936 records `{itemId, spaceId, tags}`, one tag being the game object's name
   (e.g. `28b58162-... ['HELMET', 'GEAR', 'COSTUME_M_MILITARY_GREY_94', 'SMT_GEN', 'STEEP', 'COSUTME', ...]`). The
   GUIDs are in none of the archives (decompressed), the exe dump, or the game folder: the game keeps the list in
   an encrypted cache (most likely `Documents/My Games/STEEP/Default/tmp/f6849c62/74c57e6b.dat`, 308 KB, written in
   the first session) and asks the server only for its fingerprint (`items/all?fields=lastModified,sha256,md5`,
   builtin/http.rs); it downloads the list only when the fingerprint differs. Earlier tries served `{}`, which
   logged the game out. Next experiment: serve the **whole** list (this dump) plus one record for the new item
   (a fresh GUID, tags `COSTUME_MF_POWDER_TEST_01`, `COSUTME`, `GEAR`, ...) under a new fingerprint, and a free
   offer for it; the download's exact format (and whether sha256/md5 must match the body) is unknown.
   **The download (2026-10-27, from the unpacked exe):** `items/all` without `fields` answers
   `{"compressedItems", "lastModified", "sha256", "md5"}` (all required strings; JobGetDataAll), `compressedItems` =
   base64(zlib(JSON)) (JobGetDataDecompressed: base64Decode, zlibInit 1.2.8, decompressData, extractData), the JSON
   most likely a bare array (extractData names no key; G) of items `{itemId, spaceId, type, nameId, duration,
   revision, lastModified, tags, obj}` (+ optional `maximumQuantity`; the parser's field table, 0x1b996e0 in the
   2026-10-26 dump). The game's cache is AES-like encrypted (size a multiple of 16), so the original list cannot be
   read from it. Served by the Rust server when `<data>/item_list.json` exists (assets::item_list; fingerprint =
   sha256/md5 of the JSON, G); `tools/trainer/itemlist.py` adds custom items to it and to the store.
   **The game's copy, read structurally** (`storemap.py --item-list`, 2026-10-27): the SDK keeps 0x88-byte records
   (a u64 0x320D000E inside; itemId, spaceId, **type**, **nameId**, revision, date before it, the tags vector and obj
   after it): 3,840 items (2,178 GEAR, 1,648 PROGRESSION_TOKEN, 10 TOURNAMENT_REWARD, 4 TICKET), every one with its
   nameId = the game object's name (e.g. `COSTUME_M_MILITARY_GREY_94`, tags SMT_GEN STEEP COSUTME HELMET JACKET PANT
   GLOVE BACKPACK BASIC). The earlier heuristic dump mistook type and nameId for tags. The beaver test failed with
   type PROGRESSION_TOKEN (an entry of the game's own list); a custom entry is a copy of a similar GEAR entry.
   TRIED 2026-10-27: the game downloads and caches the list, then logs out (DE-010-02, offline mode) within half a
   second - also when it builds the catalogue from that cached copy (same fingerprint, no download), so the content
   is refused, not the transfer. Cause: every item was sent with `obj: {}`. The game's records hold obj as a cJSON
   tree: `{"gameId": <the object's resource id>, "Token": 0, "price": <credits>}` (e.g. COSTUME_M_MILITARY_GREY_94:
   gameId 1017655268344 = 0xecf0fb13f8, price 56000) - **gameId is the link from the GUID to the game object**, not
   nameId. storemap.py --item-list now reads obj (3,830 of 3,840 items have a gameId); itemlist.py --id sets the
   custom item's gameId (price 0). Next try: the list with real objs + COSTUME_MF_POWDER_MASTER_CHIEF.
   If the game takes a custom list it replaces its cached copy: keep serving a list from then on (without the file
   the server sends Ubisoft's fingerprint, the game downloads, and the empty reply logs it out), or restore the
   backed-up cache folder.
   Back up `Documents/My Games/STEEP/Default/tmp/` before the game first takes a custom list (it overwrites its copy).
3. **Drawing it.** The new tag -> new Clothing branch -> new selector -> new entity in DataPC_extra_chr_patch_03.
   If the rider shows the default clothes instead, the selector side was not found (registry, list order, or the
   template copies); if the game crashes when the costume is equipped, suspect the entity / material clones.
4. Name and icon: the source's (localization is not decoded; type 0x6E3C9C6F G).
5. A save that holds the new item id after the mod is removed: unknown. Un-equip it before turning the mod off.
