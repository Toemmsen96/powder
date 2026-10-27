# world_test: the first world-placement mod

This mod tests whether edited **world cells** work in the game: whether an object can be moved, and whether a
new instance of an existing prop can be added. It changes the **WhiteRoom**, the small map behind the main menu and
the rider screens (`DataPC_STP_WhiteRoom`: the rider stands in a snowy clearing with firs, rocks and a chapel). It
makes three changes near the rider's spawn point, the map origin:

| what | cell (base) | object | from | to |
|---|---|---|---|---|
| **new fir**, 20 m tall | `Cell00036_DataBlock` `0xd8d3d91a4b` | copy of `STP_WOR_GEN_VEG_Sapin_Medium_S4_01_C_006` (`0xd8d3d96665`), new id `0x1f002400000` | - | (2.3, 12.0, -0.55): 12 m north of the spawn |
| **new fir**, 9.5 m tall | `Cell00027_DataBlock` `0xd8d3d91a42` | copy of `STP_WOR_GEN_VEG_Sapin_Small_S4_01_C_016` (`0xd8d3d9730d`), new id `0x1f001b00000` | - | (-9.0, -3.0, -2.0): 9.5 m west |
| **moved boulder**, ~3 x 3 x 5 m | `Cell00028_DataBlock` `0xd8d3d91a43` | `STP_GEN_ROCK_Granite_Small_N_01_A_008` (`0xd8d3d94147`) | (12.6, -32.8, -1.0), 35 m away | (9.5, -7.5, -1.26): 9.5 m east-south-east |

Positions are metres, Z up. The spawn (`WhiteRoomAvatarSpawnPosition`) is at (0, 0, -0.93). Each z is the ground
height there, from the ground mesh `STP_WHI_Ground_01A_LOD0` (firs stand with their origin on the ground, rocks are
sunk in by their old depth). The firs are copies of firs that stand in the same cell, so the cell already holds
their prop template, LOD meshes and collision shapes, and its prefetch list already names them.

`generate.sh` writes the three cells with `tools/forge/world.py edit` into `client/forge/DataPC_STP_WhiteRoom/`,
and the session tooling builds `DataPC_STP_WhiteRoom_patch_03.forge` from them (the newest copy of an entry wins).
The game data is generated from your own install and is gitignored.

```
mods/world_test/generate.sh                    # all three changes (default)
WORLD_TEST=move mods/world_test/generate.sh    # only the moved boulder
WORLD_TEST=copy mods/world_test/generate.sh    # only the two new firs
POWDER_CLIENT_MODS=world_test                  # in Lutris' environment, then restart Lutris
```

**Default since 2026-10-27: `WORLD_TEST=big`** (the first test was too subtle to spot): nine firs in a ring 4-12 m
around the rider (five 20 m firs north and east, four 9.5 m firs west and south-west, none between the rider and the
customisation cameras), the shipped 20 m fir 52 m north grown to 50 m (2.5x), and the boulder 6 m east of the rider at
twice its size. The z values near the spawn are estimates (G): a fir may stand slightly sunk or float a little.
`WORLD_TEST=both` is the original three-change test.

## What you should see

In the main menu and the rider customisation screens, look around the rider. The WhiteRoom has four
customisation cameras a few metres south of the spawn (`Cam_Perso_01..04` at about (1..2.3, -3.8..-4.9)); if they
look along their local +Y axis, as their placement suggests (G), then:

- the camera looking **north** (`Cam_Perso_01`) has the **big new fir** dead ahead, about 4 m behind the rider and
  16 m from the camera, filling the background above the rider;
- the camera looking **west** (`Cam_Perso_02`) has the **small new fir** dead ahead, 10 m away;
- the camera looking **east-south-east** (`Cam_Perso_04`) has the **boulder** dead ahead, 9 m away;
- the camera looking south (`Cam_Perso_03`) sees no change.

Before the mod the nearest firs and rocks are 20-60 m from the spawn, so anything within 15 m is new. The other
trees, the chapel and the rest of the menu stay as they were.

| you see | meaning |
|---|---|
| both firs and the boulder | moving and adding instances work; the tool is ready for the open world |
| the boulder but no firs | a cell override loads, but a copied instance is ignored (ids? list order? see WORLD.md) |
| the firs but the boulder is still far away | unlikely; report it |
| nothing changed, no crash | the WhiteRoom `_patch_03` is not used: check powder-wrap's log for `asset mods: DataPC_STP_WhiteRoom_patch_03.forge`; the menu may also use another map's cells |
| a crash at the menu | turn the mod off; try `WORLD_TEST=move`, then `WORLD_TEST=copy`, and report which crashes |

## Verified vs assumed

Verified with tools, without running the game:
- `world.py check` parses every cell of all four maps (see WORLD.md). Every DataBlock rebuilds byte for byte, and
  an edit without operations returns the original cell: built with `forge.py build --like`, its decompressed
  header and body blocks equal the shipped ones byte for byte.
- The three edited cells parse again. The copies differ from their originals only in the 77 renumbered ids and the
  12 translation bytes of the matrix. The moved boulder differs only in its matrix. Its proxies in neighbouring
  cells were searched for; it has none.
- `forge.py build ... --like DataPC_STP_WhiteRoom.forge` of the three folders gives an archive that passes
  `forge.py verify --decompress`. Next to the shipped archives (symlinked into a scratch folder), `forge.py stack`
  shows `<- ..._patch_03` for the three cells, and `world.py check`/`find` on that stack see the firs and the
  boulder at their new places.

Assumed (G), to be tested in-game:
- The menu scene is the WhiteRoom (its cells hold the menu pages' anchors, `Cam_Perso_*`, the avatar spawn and the
  drone) and the game reads its `_patch_03` like the other `DataPC_STP_*` archives.
- New ids in an unused range (0x1F0_0000_0000 and up) are accepted, as is an object list that grew by one.
- Collision: the copies carry their own physics component. The big fir stands 2.3 m from the border of cell 35,
  where the shipped data would have a proxy (a collision stand-in) for it; no proxy is added, so in the menu (where
  nobody rides) this does not matter.
