# world_editor: edit the world from the overlay menu

A **World editor** panel in the in-game overlay (Home, Mods tab) to add objects to the world, and to move, turn,
scale and remove the ones that are there. The edits are kept by the server. Steep loads its world cells from its
archives, so an edit shows when the game **reads its cell again**: with the live helper (below), ride a few hundred
metres away and back; without it, at the next game start.

| part | file | what it does |
|---|---|---|
| client (panel) | `src/world_editor.cpp` -> `client/world_editor.dll` | position, cell, nearby objects, add / move / turn / scale / remove, edit list with undo |
| server | `mod.lua` | keeps the edit list (`/_powder/world_editor/...`, in the server's database) |
| session step | `session.sh` -> `apply.py` | at each game start: the edits -> edited cells (`tools/forge/world.py apply`), and the panel's object index (`world.py index`) |
| live helper | `live.py` (started by `session.sh`) | for the session: each change of the edit list -> the changed cells, in `powder_mods/world_editor/live/` |
| live edits | `src/live.cpp` (in the DLL) | serves those cells in place of the archives' bytes as the game reads them (ntdll file hooks) |

## Enable it (Linux, powder-wrap)

```
native/build.sh overlay && native/build.sh mods     # the overlay and client/world_editor.dll
```

In Lutris' environment for Steep (then restart Lutris):

```
POWDER_OVERLAY=1
POWDER_CLIENT_MODS=world_editor        # with other mods: comma-separated; not together with world_test (same cells)
```

Not together with `asset_trace` either: both hook ntdll's file functions, and a function is hooked once (whichever
loads second gets no hooks; the panel says "live edits off: could not hook ...").

The Rust server (the default) runs `mod.lua`; the Python server runs no Lua mods, so with it the panel cannot store
edits, and a session uses the last list it fetched. Optional:

- `WORLD_EDITOR_MAPS=Alpes,WhiteRoom` - the maps whose index is built and put beside the game (default all four).
  The first game start indexes each map once (~20 s for an open-world map, cached in `~/.cache/powder/world_editor`
  until a game update changes the archives).
- `WORLD_EDITOR_ADDS=0` - leave the added objects out (keep the moves, turns, scales and removes), see below.
- `WORLD_EDITOR_LIVE=0` - no live helper: edits show from the next game start only (the DLL then hooks nothing).

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
7. **See it**: under the edit list the panel says `live: N cell(s) replaced` once the helper has written the cells
   (a second or two; `live: writing the cells of edit #N...` until then). Ride ~300 m away and back (or further on
   an open-world map): the edit shows when its cell streams in again. *live cells* lists each cell, whether it was
   fitted into its entry ("same size") or needed the index redirected ("bigger"), and `read N time(s)` once the game
   has read it again.
8. At the next game start everything is in the session's `_patch_03` anyway: the session log
   (`$XDG_RUNTIME_DIR/powder-wrap.log`) shows `mod world_editor: session step done` and
   `asset mods: DataPC_STP_<Map>_patch_03.forge (N entries)`.

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
- **Live helper** (`live.py run`, started in the background by `session.sh` when the overlay is on; stops when the
  session's runtime folder goes, or after 3 minutes without the server): asks the server for the list when the panel
  pokes it (`live/poke`, after each edit the server took) and once a minute otherwise (edits made elsewhere);
  per changed map it runs `world.py apply` (~0.2 s for the WhiteRoom, ~1.5 s for the Alps) and compares every cell
  it writes, and every cell any list changed before (also the session's own, which are in `_patch_03`), with what
  the game reads for that entry: the newest copy in the stack, `_patch_03` included. A cell that differs goes into
  `powder_mods/world_editor/live/` as a whole datafile, fitted to exactly the entry's size when it can be (forge.py
  `fit_datafile`: recompressed, then padded with empty zstd blocks; "same"), else as it is ("grow"), and into
  `manifest.tsv` (written last; its format is in live.py's docstring). An undone edit makes its cell the shipped one
  again. `live.py once --base A --edits B --out DIR` does one step by hand.
- **Live edits in the game** (`src/live.cpp`): the DLL polls the manifest (1 s) and hooks ntdll's `NtReadFile`
  (plus `NtCreateFile`/`NtOpenFile` for the names, `NtSetInformationFile` and `NtRemoveIoCompletion[Ex]` for
  completion ports) once a helper runs. A read of an archive that overlaps a served cell is done privately, the
  cell's bytes are copied over the archive's, and the read is completed for the game as Windows would (event, APC,
  or completion packet), so the game never sees unpatched bytes. "grow" cells: the game holds each archive's index as
  one array of `{u64 offset, u64 id, u32 size, u32}` records (`tools/forge/WORLD.md`, "The archive index in memory");
  the DLL finds the cell's record (a read-only scan, checked against its neighbours) and points it at a slot past the
  end of the archive, whose reads it answers. It writes nothing else into the game, and gives the record its own
  values back when the cell no longer needs it. The DLL logs to `powder_overlay.log`: `live: generation N`, how the
  game's reads complete (once per archive), `index record(s) found`, `index record restored`.
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

### Live edits (new 2026-10-27, not tried in-game yet)

Same setup (`POWDER_OVERLAY=1`, `POWDER_CLIENT_MODS=world_editor`, not with `asset_trace`), after
`native/build.sh mods`. powder-wrap's log should show `world_editor: live helper started` and
`world_editor live: watching the server's edits`; the panel `live: 0 cell(s) replaced ...`.

1. **Same size, WhiteRoom** (the menu scene; nothing streams out there, so this checks the menu reload): remove a
   rock near the rider; the panel shows the cell `(same size): waiting for the game to read it`. Go into the open world
   and back to the menu (or anything that reloads the WhiteRoom). Expected: the rock is gone, *live cells* says
   `read N time(s)`.
2. **Same size, open world**: in the Alps, ride to a hand-placed prop (a bench, sign, `STP_GEN_ROCK_*` rock), **Move**
   it a few metres and **Turn** it. Ride ~500 m away (watch the panel's cell number change a few times), wait a few
   seconds, come back. Expected: it stands at the new place. Then **undo**, away and back: it is back.
3. **Bigger cell**: **Add** a kicker or bench where the cell has nothing like it (a small cell: few objects in
   *Objects near you*). *live cells* should say `(bigger: index redirected)`; `powder_overlay.log` has
   `index record(s) found in N ms` (expect 1; a few seconds for the scan). Away and back: the copy is there. Undo:
   `index record restored`, away and back, gone.

| you see | meaning |
|---|---|
| `live edits off: no session helper` | `live.py` did not start: powder-wrap's log (`world_editor live:` lines), `WORLD_EDITOR_LIVE` |
| `live edits off: could not hook ...` | another mod hooks the same ntdll functions (asset_trace): load only one |
| `live: writing the cells of edit #N` for long | the helper failed on the list: powder-wrap's log, `world_editor live: error` |
| the cell never says `read N time(s)` | the game did not read that entry again (ride further; on the WhiteRoom reload the menu), or it reads it through a file the DLL does not know (report the log's `live:` lines) |
| `read N time(s)`, but no change visible | the game read the edited bytes but builds the cell from a cache, or the edit is in another copy of the cell (report the cell and the log) |
| `not redirected: its index record was not found` | the in-memory index differs from WORLD.md's (this build?): bigger edits show at the next start only |
| the game hangs while streaming, or crashes near an edited cell | a completion the DLL gave wrongly: report the log's `patched reads complete by ...` lines; `WORLD_EDITOR_LIVE=0` turns it off |
| a bigger cell shows as a hole / is missing | the game checks offsets against the file size (redirect does not work in that form): report it |

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

Live edits, verified without the game (2026-10-27; `tests/test_live.py`, `--wine` for the DLL part):
- `forge.py pad_datafile`: real cells padded by 0..300 bytes parse to the same resources, their chunks decompress
  the same through libzstd; `fit_datafile` puts edited cells into their entry's size.
- `live.py` over a scratch game folder (links to the real WhiteRoom archives) with a session `_patch_03` built the
  way powder-wrap builds it: turn + remove, an add that fits, an add that grows (a small cell), undo of the session's
  own edit (that cell served as shipped again), move + scale, clear. After each step every served cell, read back
  through `live_patch.h` (the DLL's byte-range code) in the game's aligned chunk sizes, is exactly the cell
  `world.py apply` makes; no other cell differs from what the game reads.
- `world_editor.dll` itself under Wine (scratch prefix) in a stand-in host (`tests/livehost.cpp`, MinHook like the
  overlay): every served cell read back the same through the hooks with a synchronous handle (file pointer),
  overlapped + event, completion ports bound before the DLL (learnt from dequeued packets; also with
  FILE_SKIP_COMPLETION_PORT_ON_SUCCESS) and after it, GetQueuedCompletionStatusEx, ReadFileEx + APC; no packet
  twice or left over; the grown cell's record (a game-like 24-byte record array in the host) redirected, read from
  its slot, and restored when a later generation no longer serves it.
- `session.sh` + `live.py run` against a scratch Rust server: the helper starts, follows a move, an add and an
  undo posted to `mod.lua` (each followed by a poke, as the panel does) within ~1 s, and stops when the runtime
  folder is removed.
- The in-memory index (WORLD.md): read from the running game (`/proc/<pid>/mem`, read only).

Assumed (G), to be tested in-game: that the game builds a cell from the bytes it reads each time (not from a cache);
that pointing a record moves where the game reads (and that it does not check offsets against the file's size); how
the game's archive reads complete (the DLL handles events, APCs and completion ports, and logs which one it saw).

Assumed (G), to be tested in-game: the trainer's player position is in the map's world coordinates (Z up); the
direction of riding is a fair "in front"; edited cells load like world_test's; copies outside world_test's case
(another cell, the open world, scaled) render and collide; moved objects within their cell's square still stream.
