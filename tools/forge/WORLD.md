# Steep's world placement (cells)

`world.py` reads how a map places things: the world resource's cell tables, the cell datafiles, and the objects
each cell places (props, buildings, collision stand-ins, scatter data). It finds every placement of a prop and
writes a modified cell, with an object moved, turned, scaled, removed or copied, as an unpacked datafile folder.
`forge.py build --like <map>` packs such folders into a `_patch_03`, the way asset mods ship (see `mods/world_test`).
It uses `forge.py` from the same folder and the standard library. Archives are only read, and the newest copy of
each entry is used, as the game loads it.

```
world.py grid  <map>                                   # world: cell tables (layers), levels, extent
world.py cells <map> [--at X,Y] [--level L] [--count]  # cell datafiles: index, level, square, archive (+ kinds)
world.py show  <map> CELL [--json] [--deps] [--points] [--dir FOLDER]   # a cell's objects: pos, yaw/pitch/roll, prop
world.py find  <map> 0x<prop id> | <name substring>    # every placement (prop id: header scan, ~1 s; name: all cells)
world.py edit  <map> CELL -o OUT [--move OBJ X,Y,Z] [--offset OBJ DX,DY,DZ] [--yaw OBJ DEG] [--scale OBJ S]
                              [--remove OBJ] [--copy OBJ X,Y,Z[,YAW]] [--source CELL] [--layer N]
world.py check <map> [--ids]                           # parse every cell, rebuild every DataBlock, statistics
```

`<map>` is the base archive, e.g. `"$G/DataPC_STP_Alpes.forge"`; its `_patch_NN` are added, and archives a
powder-wrap session put there (`.powder-assets`) are left out. `OBJ` is an object's id or exact name. `edit` also
writes the neighbouring cells whose collision stand-ins (proxies) follow a moved or removed object.

```
$ world.py find "$G/DataPC_STP_Alpes.forge" 0xa491258d55        # the prop STP_ALP_R03_BUI_Church_01A_Body
L0 Cell01135  0x00efadaa0a8f  group    STP_ALP_R03_BUI_Church_01A_Group_010  pos 6146.55 -7171.34 313.51  yaw  34.4 ...
L0 Cell01388  0x00efada9b379  group    STP_ALP_R03_BUI_Church_01A_Group_009  pos 5694.82 -6827.54 391.64  yaw -51.9 ...
... (11 placements)
$ world.py show "$G/DataPC_STP_WhiteRoom.forge" 35
L0 Cell00035_DataBlock (0xd8d3d91a4a, DataPC_STP_WhiteRoom_patch_02.forge): level 0, x -128..0, y 0..128; 31 listed ...
  0x00d8d3d93164  proxy    (no name)                         pos  1.33 36.38 3.51  yaw 140.0 pitch -65.0 roll -85.0  for 0xd8d3d930ab
  0x00d8d3d932af  group    STP_ALP_R01_BUI_Chapel_01C_S_001  pos -42.00 39.50 1.00  yaw 20.0 ...  prop STP_ALP_R01_BUI_Chapel_01C  children 2
  0x00d8d3d94fee  entity   STP_GEN_ROCK_Granite_Medium_N_02_A_009  pos -12.76 26.16 3.51  ...  prop STP_GEN_ROCK_Granite_Medium_N_02_A
$ world.py edit "$G/DataPC_STP_WhiteRoom.forge" 36 -o mods/x/client/forge/DataPC_STP_WhiteRoom --copy 0xd8d3d96665 2.3,12,-0.55
  copied STP_WOR_GEN_VEG_Sapin_Medium_S4_01_C_006 as STP_WOR_GEN_VEG_Sapin_Medium_S4_01_C_PWD000 (0x1f002400000, 77 ids renumbered) at (2.30, 12.00, -0.55)
```

**C** = confirmed: holds on every cell of the four maps (`world.py check`, numbers at the end) or on the samples
named. **G** = a guess, or an observation whose meaning is unknown. Nothing here has been tried in the game yet:
`mods/world_test` is the first test. All integers little-endian, positions in metres, Z up.

## The picture

```
STP_Alpes (world, 0xFBB63E47)                 one per map, in the map archive's newest patch
 +- layer 0xAFE5F9B4 + settings 0x7DB083ED   cell size 128 m, origin (-8192, -8192), 128 cells per side
 |   +- cell table 0x0E2F4444                 21,845 slots = 8 quadtree levels, finest first -> DataBlock ids
 +- layer (no table)
 +- layer + settings + cell table             a second, small table (patch-era objects; 118 cells in the Alps)
CellNNNNN_DataBlock (cell datafile)           one entry per filled slot
 +- DataBlock 0xAC2BBF68                      the list: placed objects first, then data objects
 +- placed objects                            entity 0x0984415E, group 0x3F742D26, proxy 0x1CBDE084: world matrix each
 +- data objects                              CellID points 0x5730D30E, GIProbes 0x88326C5A, scatter rules/layers
 +- dependencies (inline copies)              prop templates 0x51DC6B80, shape refs 0x2D675BA2, prefabs, materials
separate entries                              LOD meshes 0x415D9568, collision shapes 0xB22B3E61, textures:
                                              named in the cell's prefetch blob, streamed with the cell
```

## World and cell tables

- The world resource (type 0xFBB63E47, `STP_Alpes`, `STP_Japan`, `STP_Alaska`, `STP_WhiteRoom`) is the only
  entry of its type in a map stack (C). It starts with u64 id, u32 type, u8 1, u8 3, then three **layer** objects
  (class 0xAFE5F9B4), each with a settings object (0x7DB083ED) (C on the four maps). The open-world maps have a cell
  table in the first and the third layer, the WhiteRoom in the first only; the second layer never has one (C).
- **Layer settings** (0x7DB083ED, offsets from its class hash): +4 u16 (640 for the first layer, 1920 or 1536 for
  the third, 95 for the second, 0 in the WhiteRoom; G: a streaming distance in metres?), +6 u16 **cell size** =
  128, +10 i32 **min x**, +14 i32 **min y**, +22 u32 **cells per side** at the finest level (C on the four maps:
  (-512, -512) and 8 in the WhiteRoom; (-8192, -8192) and 128 in the Alps, Japan and Alaska, which are 16,384 m
  squares centred on the origin). `world.py grid` reads these.
- **Cell table** (class 0x0E2F4444): u8 1, u32 levels, u32 slots = (4^levels - 1) / 3, then slots x 21 bytes
  `u64 0, u32 0x3435AD40, u8 0, u64 DataBlock id` (0 = empty). The slot index is the cell index, the NNNNN of
  `CellNNNNN_DataBlock` (C: the DataBlock stores the same index).
- **Slot order** (C): a quadtree stored **finest level first**, each level row-major from (min x, min y). Level 0
  has side^2 cells of 128 m, level k has (side / 2^k)^2 cells of 128 * 2^k m, the last slot is the root. Alps: 8
  levels, cells 0..16383 are 128 m, 16384..20479 256 m, ..., 21844 is the 16 km root. Evidence: 99.85 % of the
  entity and group origins in level-0 cells (55,279 of 55,363) lie inside the cell's square +-8 m; the rest are
  big buildings near a border (the editor seems to file them by their bounds, G).
- Coarser levels hold what is too big or too global for a 128 m cell: the root holds the outskirt clouds (which lie
  up to 9 km outside the map) and other always-loaded objects (the Alps root: 414 entities, 385 groups, 30 MB), level
  3-4 cells hold the scattered vegetation (below). Objects in coarse cells need not lie inside the cell (C).
- **Two tables per open-world map**, so two cells can share an index and a name (C: Alps `Cell00327_DataBlock` is
  both `0xfa89b004b` in table 0 and `0x10ec93db94e` in table 1). The tool numbers the tables in file order, keys
  cells by (table, index), shows `L0`/`L1`, and `--layer N` picks table N. Table 1 is small (Alps 118 cells, Japan 1, Alaska 5) and holds patch-era objects
  (e.g. `STP_ALP_GEN_FUR_ElectricMetalBeam_01A`; G: an overlay layer added by a patch).
- Alaska has 271 `CellNNNNN_DataBlock` entries no table points at (G: removed by a patch). Maps also hold
  `DataBlock_*` entries outside the tables (missions, drop zones: `DataBlock_STEEP_ALP_ONBOARDING`, ...; G: loaded by
  gameplay, not by streaming). After the table comes u32 (side^2) and side^2 u16 (0x8002 mostly; G: per-cell flags).

## Cell datafiles

A cell is an ordinary datafile (README.md): resource 0 is the DataBlock, then the listed objects in list order,
then the dependencies (C: resources follow the list in every cell whose objects all exist).

**DataBlock** (type 0xAC2BBF68, C on all 54,577 cells): u64 id, u32 type, u8 1, u32 n, n x (u8 1, u8 0, u64 object
id), u32 **placed**, u32 cell index, u32 0. The first `placed` objects are the placed ones (entities, groups,
proxies, and one 0x8DEEB8AD per map); the data objects follow (C). The placed ids ascend in 99.3 % of cells (G:
sorted by the editor). 19 listed ids (18 in Japan, 1 in the Alps) exist in no datafile of the map (C; the game
evidently tolerates dangling entries).

### Placed objects

| class | what | count (4 maps) | matrix at |
|---|---|---|---|
| 0x0984415E | **entity**: one placed thing (prop, light, camera, FX, spawn point) | 51,849 | +14 |
| 0x3F742D26 | **group**: a placed prefab (building, `HAS_PackedInstance_NNNN`) with child entities | 8,725 | +14, and each child's at child + 14 |
| 0x1CBDE084 | **proxy**: a collision stand-in for an entity/group listed in another cell | 4,456 | end - 72 - 9n |

**World matrix** (C): f32[16], rows = the object's local X, Y, Z axes in world space (scaled), row 4 = translation,
last column (0, 0, 0, 1); a point maps as `p_world = p_local x M` (row vector). Checked: 31 of the WhiteRoom's 45
medium and small firs have their origin within 2 cm of the ground mesh (`STP_WHI_Ground_01A`, decoded with
mesh.py; the others are sunk deeper), rocks are sunk 0.8-1.3 m. The scale is in the row lengths (1.0 almost
everywhere). The tool prints yaw (about Z from +X), pitch and roll, which assume an orthogonal basis.

**Entity** (0x0984415E; layout C on the sampled rocks and firs, G beyond): u64 id, u32 class, u8 1, u8 3,
**f32[16] matrix**, u32 n, n x (u8 4, component), then trailing objects. Components seen:
- 0xEC658D29 (G: visual): links the **prop template** (0x51DC6B80) and holds one 0x536E963B per LOD, each linking
  that **LOD mesh** entry (C on the rock: its 4 links are exactly `STP_GEN_ROCK_Granite_Medium_N_02_A_LOD0..3`).
- 0x9B52CA4D (G: render settings; firs nest a 0xB3E5916C and 64 x 0x33CE8E70 in it, G: foliage/wind).
- 0x0E5A450A (G: physics) with a 0x228F402A body; it links the **collision shape** (`u8 1, u8 1, u64 id`): a
  0xB22B3E61 entry directly (rocks: `STP_GEN_ROCK_Granite_Medium_N_02_A`) or a shape list 0x2D675BA2 in the cell
  (firs: `..._Sapin_Baby_S4_01_C_LST`, three sub-shapes: the mesh shape entry and two 0xB8599052 primitives with a
  physics material link, `CM_Wood_Tree`).
- trailing: 0x1C4B22AA, **0x4AEC3476 bounding box** (f32 min[3], max[3] around the origin, world-axis-aligned; C
  on one rock: it encloses the rotated LOD0 bounds with ~0.4 m to spare), 0x60121A9E, 0xDB1D406E.

**Group** (0x3F742D26): the same head (matrix at +14), components per part (visual components linking the parts'
prop templates, e.g. `..._Church_01A_Body`, `_Wing`, `_RFS_Large`), a physics component linking the group's
**Merged Shape** (0x2D675BA2 in the cell -> 0xB22B3E61 entry), and **child entities** (nested 0x0984415E objects,
each with its own world-space matrix; 6,852 in the Alps). The cell also carries the prefab (0xE802B9DA, e.g.
`STP_ALP_R03_BUI_Church_01A_Group`; G: the template the group was made from). `HAS_PackedInstance_NNNN` groups
have an identity matrix and are 300 KB-1 MB: baked scatter (below).

**Proxy** (0x1CBDE084, C): links a shape ref (0x2D675BA2, kept in the proxy's own cell), and ends with f32[16]
world matrix (= its target's), f32 1, u32 n, n x (u8 0, u64 **target** id). A target is a listed entity or group
(Alps 83 %, Japan 89 %) or a group's child entity (Alps 16 %, Japan 3 %), the rest are in no cell (G: deleted
objects); almost always in **another** cell (C): of those in the Alps, 92 % on the same level one cell away, 5 % in
a finer cell under the proxy's, a few 2-3 cells away, 54 in the other table. So an object near a cell
border has stand-ins in the neighbours its collision reaches into (G: each cell's physics world is built from its
own list), and moving or removing it means moving or removing those too: `edit` does that for the cells around it
(`--proxy-radius`, default 1, plus the coarser cells over them), in the same table.

### Props, meshes and collision

- **Prop template** (0x51DC6B80, 207 bytes; MESHES.md calls it the entity): its LOD mesh ids and LOD distances. It
  is copied inline into every cell that places it (C: `find` by its id is a header scan). The cell header lists it,
  so a prop id finds all its placements (`find 0x<id>`, ~1 s for a map).
- **Meshes** (0x415D9568) and **collision shapes** (0xB22B3E61) are entries of their own, not inside the cell. The
  cell's **prefetch blob** (README.md) names them: for WhiteRoom `Cell00035` exactly the LOD meshes, shape entries,
  imposter textures and FX textures of the props it places (C). So a cell can only show props whose meshes its
  prefetch blob names; `edit --copy --source` merges the source cell's blob in.
- Shapes are per prop (rocks, firs) or per prefab (`... Merged Shape`, one per building group), so collision follows
  the instance's matrix (C on layout, G on behaviour).

### Data objects (after the placed prefix)

| class | name | per cell | G: what |
|---|---|---|---|
| 0x5730D30E | `CellID=N` | ~5 in level-0 cells | points: header (sub-object 0x4AEC3476 box) then u32 0, u32 n, n x 44 bytes `u64 id, u32 0xDB30CAB0, f32 pos[3], f32 scale, f32 quaternion[4]` (C). On a 10 m grid at ground height, tilted with the slope, scale 1, no prop link: ground-cover or terrain sample points (G) |
| 0x88326C5A | `GIProbes_N` | 1 in each level-0 cell | lighting probes (G) |
| 0xC29A81B3 | `NormalSlope3 #3`, `snowtree2 #17`, `forest5 #42` | coarse cells | scatter rules (G) |
| 0x44F7C7A6 | `more4 #4`, `less2 #6` | coarse cells | scatter layers (G) |

**Vegetation in the open world is not placed one by one**: the Alps' firs, bushes and scattered rocks live in
`HAS_PackedInstance_NNNN` groups in level 3-4 cells (1-2 km), next to template entities of the scattered props (an
identity-matrix 0x0984415E per prop, e.g. `STP_WOR_GEN_VEG_Ghost_Medium_S4_01_D`) and the rules/layers above. The
packed format (quantised, 300 KB+) is **not decoded**. The WhiteRoom's firs and rocks, and the open world's
hand-placed props (buildings, furniture, signs, walls, lights, ski-lift parts, rocks named `STP_GEN_ROCK_*`), are
ordinary entities and groups.

## Ids

- Ids are 64-bit; the shipped ones are < 0x1BD_5300_0000 (C: entries and nested objects of all four maps). Editors
  hand out blocks: an entity's nested objects mostly take the ids right after its own (a fir's 76 nested objects lie
  between id + 1 and id + 77), patch-era edits add a second block elsewhere (C on samples).
- An instance **owns** its id and its nested objects' ids: every id-like u64 (2^32..2^42) followed by a known
  object class (`OBJECT_CLASSES`, 82 classes learned from the own-id blocks of ~6,000 cells of the four maps) and
  not a resource of the datafile (those are links). Over all 18,246 Alps cells this finds 732,837 owned ids and none
  owned by two different instances (C; ids repeat only where the same object is listed in several cells, 490
  objects, mostly scatter rules and packed instances).
- A **copy** gets the block `0x1F0_0000_0000 + (cell index << 20) + (copy number << 12)` and keeps the order of
  its original's ids (`--id`, `--copy-no` override). Every occurrence of an owned id in the payload is replaced, so
  internal links follow; links to resources (prop, shapes, materials) stay. `edit` refuses a block that clashes with
  an entry and warns about id-like values next to the instance's own block that it did not renumber.

## Editing (what `edit` writes)

- **move / offset / yaw / scale**: the matrix (and a group's child matrices) times a world-space delta; a turn or a
  scale also turns the bounding boxes (conservatively, G); proxies of the object and of a group's children, in this
  cell and around it, get the same delta. Warns when the new origin leaves the cell's square (G: streaming and
  culling go by cell; move far objects with `--remove` here and `--copy --source` in the target cell).
- **remove**: drops the object and its proxies from the lists and the datafiles (the placed count follows). Its
  prop template and shapes stay as dependencies (harmless, G).
- **copy**: entities only (groups have proxies and children to renumber; not done yet). The copy is listed at the
  end of the placed prefix (its id is the largest) and its resource follows the last placed one. `--source` copies
  from another cell and brings the dependencies its payload links to (transitively) plus the source's prefetch
  records. Copies get no proxies: near a border their collision ends at their own cell (G).
- An edit without operations writes the cell unchanged: its DataBlock rebuilds byte for byte (C: all cells), and
  `forge.py build` of it gives a datafile whose decompressed header and body equal the shipped ones (C on WhiteRoom
  Cell00035). `edit` checks every result parses, packs and keeps the placed prefix free of data objects.
- The output folders are named `<entry id>_<name>` (the entry id tells the two tables' same-named cells apart), go
  under `mods/<mod>/client/forge/<map base>/`, and the session builds the `_patch_03` (CLAUDE.md, mods/world_test).

## `world.py check` (2026-10-27)

| map | cells | listed objects | DataBlocks rebuilt | entity/group origins in their level-0 cell (+-8 m) | proxy targets found |
|---|---|---|---|---|---|
| WhiteRoom | 9 | 189 | 9/9 | 165/165 | 16/17 |
| Alpes | 18,246 | 126,204 | 18,246/18,246 | 23,661/23,683 | 2,961/3,567 (+584 group children) |
| Japan | 18,382 | 79,617 | 18,382/18,382 | 17,932/17,966 | 849/949 |
| Alaska | 17,940 | 84,803 | 17,940/17,940 | 13,521/13,549 | 630/790 |

In every cell the placed objects come first, and the placed count equals the number of non-data objects (C).

## Open questions and next experiments

1. **In-game**: does `mods/world_test` show the moved boulder and the two new firs (see its README)? That decides
   whether the game takes edited cells, new ids and longer object lists.
2. A **new instance in the open world**: copy a hand-placed prop (a bench, a sign, a rock `STP_GEN_ROCK_*`) next to
   a well-known spot in the Alps with `--copy` (or `--source` from a cell that already places it), ride into it,
   and check its collision. Then a copy of a group (renumber its children; give it proxies).
3. **Proxies for copies**: write 0x1CBDE084 stand-ins into the neighbour cells a copy's bounding box reaches (the
   shape ref is the same 0x2D675BA2 the original's proxies use).
4. **Packed instances** (`HAS_PackedInstance_NNNN`): decode them to move or remove trees in the open world.
5. The `CellID` points (0x5730D30E): what they place (dump a few next to the terrain mesh).
6. Entity component layout in full (render settings, the trailing objects), the layer settings' first u16, the
   per-cell u16 after each table, the 0x8DEEB8AD object (one per map, in the root cell).
7. Far LODs: whether a moved object also lives in baked far-LOD data (`LodData`, 4 MB in the Alps world datafile,
   `STP_ProxyLOD`), so that it would still show at its old place from afar.

## Related work

- Sean Shmulevich's [steep-gameplay-research](https://github.com/Sean-Shmulevich/steep-gameplay-research) (gameplay
  physics, animation, a native mod host) imports specifications of the **terrain** files `PC*_terrainlin*.tbf`:
  the height/class/material codec, the node lookup and FloorHeightData
  (`docs/reconstruction/MAP_DECODING_UPDATE.md` and `docs/reconstruction/studio-map/`), with reported bit-exact
  decoding of 1,541 tiles. world.py covers placed objects, not terrain. A terrain reader or editor for the map
  builder should start from those specifications. That repository has no reuse license (its NOTICE.md): read it as
  a reference and credit it; don't copy its code.
