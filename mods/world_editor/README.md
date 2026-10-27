# world_editor: edit the world from the overlay menu

A **World editor** panel in the in-game overlay (Home, Mods tab) to add objects to the world, and to move, turn,
scale and remove the ones that are there. The edits are kept by the server and take effect at the **next game
start**: Steep loads its world cells from its archives, so nothing changes while you play (no live preview yet).

| part | file | what it does |
|---|---|---|
| client (panel) | `src/world_editor.cpp` -> `client/world_editor.dll` | position, cell, nearby objects, add / move / turn / scale / remove, edit list with undo |
| server | `mod.lua` | keeps the edit list (`/_powder/world_editor/...`, in the server's database) |
| session step | `session.sh` -> `apply.py` | at each game start: the edits -> edited cells (`tools/forge/world.py apply`), and the panel's object index (`world.py index`) |

## Enable it (Linux, powder-wrap)

```
native/build.sh overlay && native/build.sh mods     # the overlay and client/world_editor.dll
```

In Lutris' environment for Steep (then restart Lutris):

```
POWDER_OVERLAY=1
POWDER_CLIENT_MODS=world_editor        # with other mods: comma-separated; not together with world_test (same cells)
```

The Rust server (the default) runs `mod.lua`; the Python server runs no Lua mods, so with it the panel cannot store
edits, and a session uses the last list it fetched. Optional:

- `WORLD_EDITOR_MAPS=Alpes,WhiteRoom` - the maps whose index is built and put beside the game (default all four).
  The first game start indexes each map once (~20 s for an open-world map, cached in `~/.cache/powder/world_editor`
  until a game update changes the archives).
- `WORLD_EDITOR_ADDS=0` - leave the added objects out (keep the moves, turns, scales and removes), see below.

## Use it

1. In-game press **Home**, open the **Mods** tab, the section **world_editor: World editor**.
2. Pick the **map** you ride in (Alpes, Japan, Alaska, or WhiteRoom = the main menu's scene). The panel cannot tell
   which map is loaded; it starts on the map of your newest edit.
3. The top shows your position and the 128 m cell you are in.
4. **Add an object (experimental)**: filter the list (`sapin` = firs, `rock`, `kick`, `bench`, ...; `*` marks
   favourites), choose one, set *metres in front* (along the direction you last rode), *height offset*, *turn* and
   *scale*, press **Add here**. Each prop is a copy of one placed instance of it, so it keeps that instance's tilt.
5. **Objects near you**: the objects within the radius (props only; tick *also cameras, anchors, FX* for the rest -
   moving the menu's camera anchors or spawn points breaks things). Select one, then **Move** (to the position in
   the fields; *to me* / *to in front* fill them), **-90 / -15 / +15 / +90 / Turn**, **x0.8 / x1.25 / x2 / Scale**,
   or **Remove**. Moved objects should stay within their cell (the panel warns): far moves are better done as
   remove + add.
6. **Edits of <map>**: every edit, newest first, each with **undo** (undoing an added object also undoes its later
   moves); **Undo last**, **Clear all...**. The status line shows the server's answer.
7. Quit and start the game again: the session log (`$XDG_RUNTIME_DIR/powder-wrap.log`) shows
   `mod world_editor: session step done` and `asset mods: DataPC_STP_<Map>_patch_03.forge (N entries)`.

Edits are only taken from the server's own machine (the game under powder-wrap talks to it over loopback); on a
server others can reach, nobody else can change its world. `ALLOW_REMOTE` at the top of `mod.lua` lifts that (the
game's requests carry no session, so anyone could then edit).

## How it works

- **Stored edits** (`mod.lua`): `GET /_powder/world_editor/edits` gives `{"version": 1, "next": N, "edits": [...]}`;
  `POST .../edits` adds one (`add` with a source entity, or `move`/`turn`/`scale`/`remove` of a shipped object
  `obj = {layer, cell, id}` or of an added one `of = n`), `POST .../undo {"n": N}`, `POST .../clear {"map": M}`.
  Checked (maps, ops, ids, finite numbers in range, at most 2000 edits) and saved with `api.save` (the server's
  SQLite database, scope `mod:world_editor`). Each session also keeps a copy in `<data>/world_editor/edits.json`.
- **Session step** (`linux/powder-wrap` runs a named mod's `session.sh` before it builds the asset mods): fetches the
  list from the running server (falls back to that copy), runs `world.py apply <map> edits.json` per map into
  powder-wrap's per-session folder (`POWDER_MOD_FORGE`; packed into `<Map>_patch_03.forge`, then deleted; never into
  `client/`, which the server publishes to launchers), and copies the index files into `powder_mods/world_editor/`
  beside the game (removed with the session).
- **world.py apply** folds the list (later edits act on where earlier ones left an object), writes one cell per
  changed cell (moves and removes also update the object's proxies in the neighbouring cells), puts each added copy
  into the finest existing cell at its position (ids `0x1F0_0000_0000 + (cell << 20) + (k << 12)`, as `edit --copy`),
  and skips (with a warning) what no longer matches the archives.
- **world.py index**: every placed entity and group (id, cell, position, yaw, scale, bounding box, name, prop), one
  tab-separated line each; placeable props are the entities with a prop template (one example instance each).

By hand (outside a session): `apply.py generate --edits <edits.json> [-o DIR]` writes the cells to `generated/`
(gitignored) to look at with `world.py show <map> <cell> --dir <folder>`; `apply.py index --out DIR` builds indexes.

## What to test in-game

1. **The panel**: it appears in the Mods tab, the position changes as you ride, the cell number fits (WhiteRoom:
   the rider stands near (0, 0, -0.9), cell 27/28/35/36 around the origin; G: that the trainer's position is the
   map's coordinates). Nearby objects listed with sensible distances (the WhiteRoom's firs and rocks 20-60 m away).
2. **Remove** (no copies involved): in the WhiteRoom, remove the 36 m fir `STP_WOR_GEN_VEG_Sapin_Big_S4_01_C_001`
   (26, 63) or a rock; restart; it is gone. Then **Move** a rock next to the rider, **Turn** and **Scale** it.
   `WORLD_EDITOR_ADDS=0` keeps any added objects out of this test.
3. **Add** (experimental): add a fir (`Sapin_Medium`) 6 m in front in the WhiteRoom, restart. Copies of firs render
   (mods/world_test, 2026-10-27) but showed a long thin white diagonal bar across the menu scene whose cause is not
   known yet: expect it with copies until world_test's bisecting finds it.
4. **Open world**: a remove or move of a hand-placed prop (a bench, a sign, a `STP_GEN_ROCK_*` rock) in the Alps,
   ride to it after a restart; then an add of a kicker or bench, and whether you collide with it.
5. **Undo**: undo an edit, restart, it is back as shipped.

| you see | meaning |
|---|---|
| "no index for <map>" | the session step did not run or that map was not indexed: powder-wrap's log, `WORLD_EDITOR_MAPS` |
| "not found (the server runs no mods/world_editor ...)" | the Python server, or mod.lua not loaded (server log: `loaded mod world_editor`) |
| "the world is edited from the server's own machine only" | the game reached the server from another address |
| an edit has no effect after a restart | the log's `world_editor:` lines: a warning names the skipped edit |
| the edit shows, but somewhere else / tilted | report the object and the panel's position (coordinates or tilt assumptions) |

## Verified vs assumed

Verified without the game (2026-10-27):
- `world.py edit` writes byte-identical cells after its refactor (copy step shared with `apply`; WhiteRoom copies,
  `--source` copies, moves, scales, removes compared with the previous version).
- `world.py apply` on WhiteRoom and Alps edit lists: adds (same cell and from another cell), edits of added objects,
  moves/turns/scales/removes of shipped ones (with proxies), undone adds, malformed or out-of-range edits skipped;
  the cells pack with `forge.py build --like` (`verify --decompress`: no problems) and read back through a scratch
  stack with the objects where they should be. `world.py index`: WhiteRoom 172 objects, Alps 24,278 in 20 s.
- `mod.lua` on a scratch Rust server: every endpoint, the checks (bad map, op, id, scale, `of`, body), undo of an add
  with its dependent edits, clear, persistence across a restart.
- The panel (`world_editor.cpp`) under Wine with a fake host (ImGui without a renderer, a fake position, clicks by
  label): index loading (WhiteRoom, Alps, a missing Japan), adding, selecting, move / turn / scale / remove, undo,
  clear; the request bodies it sent were replayed to the scratch server (all accepted) and applied.
- powder-wrap's asset block with the new session step, run against a scratch game folder (symlinks to the real
  archives): both `_patch_03` archives built and listed in `.powder-assets`, the index files in
  `powder_mods/world_editor/`, the per-session folders deleted.

Assumed (G), to be tested in-game: the trainer's player position is in the map's world coordinates (Z up); the
direction of riding is a fair "in front"; edited cells load like world_test's; copies outside world_test's case
(another cell, the open world, scaled) render and collide; moved objects within their cell's square still stream.
