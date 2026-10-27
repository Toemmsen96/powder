# Powder modding

A mod is a folder `mods/<name>/` with up to three parts, each optional:

| part | what | runs where |
|---|---|---|
| `mod.lua` | server mod (Lua): events, HTTP/NEX hooks, admin tools - `server/src/mods.rs` | the Powder server |
| `client/*.dll` (from `src/*.cpp`) | client mod (native): hooks, per-frame logic, overlay panels - `powder_api.h` | in the game, hosted by the overlay |
| `client/forge/<Base>/` | asset mod: `.forge` entries overriding the game's - `tools/forge/` | the game loads them from `<Base>_patch_03.forge` |

Players choose: `POWDER_CLIENT_MODS=name,name` for powder-wrap (Linux); nothing of a mod's client part reaches a game
unless named (the Windows launcher asks per server and pins the files' hashes).

## Client mods (native)

`mods/<name>/src/*.cpp` → `native/build.sh mods` → `mods/<name>/client/<name>.dll`, which exports
`powder_mod_init(const PowderApi *api, PowderMod *mod)`. The API (`powder_api.h`, version 2):

- **hooks**: `hook` (function detour + trampoline to the original), `hook_mid` (a callback at any instruction
  with all registers, changes kept), `unhook`, `patch_code`; addresses from `module_base` + RVA or `find_pattern`.
  Steep's own code is VMProtect'd: hooks on it are written from outside by `armer.exe` (a few ms each).
- **memory**: `read`/`write` (never fault), `player_position`.
- **per frame**: `on_frame`. **UI**: `add_panel` (Dear ImGui, attach with `powder_imgui_attach`).
- **server**: `http(method, "/_powder/...", body, done)` - talk to the mod's server half (`api.before_http` in Lua).
  No player or admin session is attached; the reply arrives on the mod host's network thread.

Example: `mods/asset_trace` (hooks ntdll's file calls, a panel, a log). Finding hook targets in the game:
`tools/trainer/` (`dumpexe.py` for the unpacked code, `watch.py`/`stackwatch.py` for who writes what, `ptrscan.py`).

## Asset mods

Extract an entry (`tools/forge/forge.py extract <archive> <id>`), change it, put the `.datafile` (or unpacked
folder) in `mods/<name>/client/forge/<Base>/`. powder-wrap builds `<Base>_patch_03.forge` from all named mods'
entries for the session and removes it afterwards (confirmed in-game: mods/model_swap (removed 2026-10-27, in git history)). Textures are decoded
and re-encoded by `tools/forge/texture.py` (format: `tools/forge/TEXTURES.md`; example: mods/custom_texture (removed 2026-10-27, in git history));
meshes by `tools/forge/mesh.py` (both confirmed in-game 2026-10-26) (OBJ export/import, also with a new topology; `tools/forge/MESHES.md`; example:
mods/mesh_edit (removed 2026-10-27, in git history)). Models from Blender: `mesh.py export -o x.glb` / `import --glb` with armature and weights
(`tools/forge/BLENDER.md`; example: mods/shoulder_pads (removed 2026-10-27, in git history)). Which datafiles hold a mesh or texture:
`forge.py where <game folder> --id <resource id>`. Swapping whole entries/resources: `forge.py swap` (mods/model_swap (removed 2026-10-27, in git history)).
