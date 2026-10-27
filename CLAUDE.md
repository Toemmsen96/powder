# Powder — notes for Claude Code

Powder is a self-hosted stand-in for Steep's (Ubisoft) online services, so an owned copy stays playable. Two
codebases live here:

- **Python** (`powder/`, `tests/`, `plugins/`): the working server and the Windows launcher. `main` is Python only.
- **Rust** (`server/`, branch `rework/rust-server`): the server being ported, plus server/client **mods**. The
  launcher stays Python for now.

The README documents the Python server and the launcher; it is the reference for behaviour.

## The Rust rework (branch `rework/rust-server`)

Goal: port the Python server to Rust **byte for byte** (the game and Ubisoft Connect parse every reply), and add
modding (Lua server mods, client mods for the players' games).

```
cargo test                      # from the repo root; all must pass
cargo clippy --all-targets      # clippy-clean (no warnings)
cargo build && ./target/debug/powder-server --help
```

Crate layout (`server/src/`, library `lib.rs` + binary `main.rs`):

| area | files | ported from |
|---|---|---|
| config, db, repos, util, logging, assets, gamedata | `config.rs` … `gamedata/` | base commit |
| TLS + local CA, OS trust | `tls.rs`, `trust.rs` | `tls.py`, `trust.py` (+ Linux/Wine) |
| HTTPS pipeline + router | `https/mod.rs`, `https/routes_{powder,profile,ubi,ugc}.rs`, `https/websocket.rs` | `transport/https/` |
| built-in replies (no pack) | `builtin/` | `builtin/` |
| NEX transport + handlers | `nex/{connection,server,dispatch}.rs`, `nex/handlers/` | `transport/nex/` |
| Storm (UDP P2P helpers) | `storm.rs` | `transport/storm.py` |
| accounts, sessions | `core/`, `services/accounts.rs`, `services/seed.rs` | `core/`, `services/accounts.py` |
| services (`Services` in `services/mod.rs`) | `services/*.rs` | `services/*.py` |
| admin CLI | `admin.rs` + `main.rs` | `admin/cli.py` |
| Ubisoft Connect's demux | `demux/` (shares `CloudSaveService` and `ConnectClients` via `DemuxDeps`) | `transport/demux/` |
| mods | `mods.rs` (Lua), `client_mods.rs` | `plugins/` (new design) |

CLI: `run` (default), `mods`, `admin …` (all of Python's), `gamedata build|verify|zip|install`.

Python tests (`tests/`, pytest; `uv run pytest`): on Linux 4 fail before and after any change
(`test_launcher.py` run_game tests and `test_packaging.py::test_busy_port_is_detected` need Windows). The launcher's
client mods are `powder/launcher/mods.py` (+ the "Client mods" step in `session.py`, `tests/test_client_mods.py`).

### State of the port

Everything of the Python server is ported (2026-10-25): all HTTPS routes, the websocket (`/v2/websocket` and
Connect's `/v2/websocket/server`), NEX stats and matchmaking, channels, the importer, the admin CLI. Since 2026-10-26 it is the default and has carried every session
(Connect, demux, game start, NEX, stats, cloud saves, the overlay's admin tab, mods). Known gaps: `/_powder/status`
lists a game's address without its port (`NexChannel` does not know its UDP peer); entity/UGC/cloud-save files
are sent whole (no Range / If-Modified-Since); websocket permessage-deflate is declined. Each port's commit
message lists its behaviour differences. The Rust server is the default (powder-wrap and the Linux launcher, since
2026-10-26); the Python server stays available (POWDER_SERVER=python) and the reference for parity.

Toolchain: Rust 1.99.1 (latest stable as of 2026-10-26), pinned in rust-toolchain.toml; edition 2024
(`rust-version = "1.99"`, resolver 3).

## Conventions

- **Parity first.** Read the Python source (`git show main:powder/server/...`) and its tests
  (`git show main:tests/test_*.py`) before porting; port the tests with their byte vectors. JSON goes out as
  Python's `json.dumps` writes it: `util::dumps` (", " / ": " separators, non-ASCII escaped), `serde_json` has
  `preserve_order`. Python quirks matter (e.g. `round()` is half-to-even). Note any behaviour difference in the
  commit message.
- **Style:** `//!` module docs that say what and why, few terse comments, ~120-column lines, `anyhow` with
  context, log targets `powder.<area>` (the logger only prints targets starting with `powder`).
- **Commits:** one focused commit per piece, message like `Rust rework: <what> (<detail>)` plus a short body
  saying what was tested; end with the Co-Authored-By line Claude Code provides. Push to
  `origin rework/rust-server` when a piece is done and green.
- **Shared state:** the server is tokio multi-threaded; services are `Arc<…>` + `parking_lot` locks, never held
  across `.await`. `ModHost` (Lua) is not `Sync`: it sits behind `Arc<Mutex<ModHost>>`. HTTPS `handle()` is
  sync and runs in `spawn_blocking` (scrypt, SQLite, Lua).
- **Mods:** server mods are Lua (`mods/<name>.lua` or `mods/<name>/mod.lua`, `return function(api) … end`; API
  in `mods.rs`'s module doc). Client mods are `mods/<name>/client/` + `mod.toml`, served at `/_powder/mods`.
  Trust model: the launcher loads a client mod only if its player approved it, pinned to its hashes; never weaken
  that (a server must not be able to push code to players).

## Working with agents

Parallel work goes to subagents in **worktrees**. Worktrees start from `main` (no `server/`): tell each agent to
first run `git reset --hard origin/rework/rust-server`. Give each agent a disjoint set of files (say which files
it must not touch), have it commit but not push, then cherry-pick its commit here, resolve the usual one-line
conflicts in `lib.rs` / `mod.rs` lists, run the tests, remove the worktree and its branch, push.

## Testing against a scratch server

Use a config with unprivileged ports in a scratch folder, e.g.
`[server] https_port = 18443  crl_port = 18080  nex_port = 16005  storm_port = 19000`, run
`powder-server -c <that config> run`, and reach it with
`curl --resolve public-ubiservices.ubi.com:18443:127.0.0.1 --cacert <data>/tls/ca.pem https://public-ubiservices.ubi.com:18443/...`.
The Python codecs on `main` (e.g. `transport/nex/prudp.py`) are handy cross-checks.

Never run `admin trust install/remove` yourself: it changes the system trust store; leave it to the user.

## Security (reviewed and hardened 2026-10-25)

Both servers, the launcher and powder-wrap were reviewed; the fixes are in both servers alike (each Rust
commit's message is the spec the Python one followed, and the other way round). In short: with accounts,
players act only on their own data (401 without a session, 403 for others'); `[server] public` makes tickets
strict; CORS only for Ubisoft's https origins (and the server's own pages); sign-in limits count first, 4
hashes at once; body/connection/time limits on HTTPS, websocket, demux, NEX, Storm; NEX handshake in order,
rate-limited, `nex_check_signatures` (log by default until a live session confirms how the game signs);
Storm peers need their key; new CAs name-constrained to the 8 redirected hosts, keys 0600; remember-me
tickets stored hashed (90 days); game-data packs checked before they are trusted (Connect's product YAML is
always Powder's own: a pack's names/images are not shown); Lua mods sandboxed (no io/os/require, 128 MiB,
100 ms per hook; server work a mod asks for, like api.admin's hashing, is off the clock). Launcher: a
server's CA is checked (one self-signed, name-constrained CA within ubi.com/ubisoft.com) and pinned per server
(TOFU, `host#<fingerprint>` in the Join address), client-mod approvals per server CA + content, System32
programs only in elevated steps, hosts file kept byte for byte, narrower firewall rules.

Open (decide with the user before doing):
- native/: the TLS helper pins the server's own leaf certificate, and there is now a Linux build too
  (2026-10-25). tls_hook.h is the shared core: it traps OpenSSL 1.0.2n's ssl_verify_cert_chain (RVA 0x1D3E290
  in build e8b14fe6...) with a guarded page + VEH and accepts the chain only when the leaf's cached
  tbsCertificate (X509_CINF+0x50) hashes to the pin. hook_guard.c + armer.c are the Windows helper (injected by
  the launcher). version_proxy.c + version.def are the Linux helper: a version.dll the game loads (it imports
  version.dll), forwarding to vers_powder.dll and arming the hook in-process; powder-wrap deploys it into
  Steep's folder, writes powder_tls.pin from server.pem, and sets WINEDLLOVERRIDES=version=n,b. Build with
  native/build.sh (mingw/zig). Fails closed (no pin -> offline), never patches bytes (VMProtect-safe).
  CONFIRMED ONLINE 2026-10-25 (Linux/Proton, the user's PC): powder_tls.log showed the armer patch the
  function and `verify #N -> server cert pinned, accept`, and the game went online. On Linux the final
  mechanism is a byte-patch, not in-process PAGE_GUARD: VMProtect under Wine blocks the game's own
  VirtualProtect, so `armer.exe` (launched as a child by the proxy once the code is unpacked) uses
  VirtualProtectEx to write a 14-byte `jmp` over ssl_verify_cert_chain's entry, redirecting it to our pinned
  check after a short delay past the startup integrity check. The guard+VEH path in tls_hook.h/hook_guard.c
  remains the Windows helper's approach. The offsets are from a live steep.exe memory dump.
- The launcher installs per user, so its elevated hosts step runs a user-writable powder-cli.exe; a
  per-machine install (`PrivilegesRequired=admin`) would close that (needs a Windows test).
- New CAs could carry EKU serverAuth (today's are trusted by Windows for any purpose within their names),
  and the launcher could then require it.
- The game's login finds its player through "Connect signed in from the same address": players behind one NAT
  on a lenient server can be mixed up (tie the game's ticket to the Connect session if the demux allows it).
- With a pack, Connect's library shows less (Powder's own product YAML); names/images could be taken from the
  pack while the launch details stay Powder's.

## Starting on a new device

```
git clone https://github.com/Toemmsen96/powder.git && cd powder && git checkout rework/rust-server
cargo test                         # Rust (rustup/cargo needed)
uv sync && uv run pytest           # Python (uv brings its own Python; 3.14 from the system also works)
cp config.example.toml config.toml
native/build.sh                    # Linux: TLS helper, overlay, client mods (MinGW-w64 cross compiler)
mods/<name>/generate.sh            # asset mods: their entries come from your own Steep install
```

Not in git (gitignored, per device): `config.toml`, `launcher.toml`, `data/` (database, local CA in `data/tls/`,
NEX signing key, logs), `gamedata/` (game-data pack), `recordings/`. A new device makes a new local CA on first
start; copy `data/` over to keep the same players, accounts and CA. No game-data pack exists on the first
device either (the server runs on built-in replies).

## The user's setup and live test results

The user plays on Linux (Arch) with Steep installed through **Lutris** (Ubisoft Connect version, Wine prefix,
GE-Proton through `umu-run`). Lutris' Steep entry runs `UbisoftConnect.exe uplay://launch/3279` (3279 = Steep).
Paths differ per device; on the first PC the prefix is `~/Games/ubisoft-connect`, the game is in
`drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep` (exe `steep.exe`, lowercase).

**Use `linux/powder-wrap`** (README "Linux"): set as Lutris' Command prefix for the Steep entry. Per session it
makes a private user+mount+net namespace (loopback only), a private `/etc/hosts` with the Ubisoft names, starts
the Powder server inside (Python by default, `POWDER_SERVER=rust`), and puts the local CA into the prefix's root
store only for the session. No system changes, no root. Server output: `$XDG_RUNTIME_DIR/powder-wrap.log`.
It needs a Python server to run (`.venv` from `uv sync`, else `uv`, else a system `python3` with aiohttp and
cryptography) or `POWDER_SERVER=rust`. Lutris shows none of its output: a failure goes to that log and a desktop
notification ("the game ran for a very short time" in Lutris' log means the wrapper stopped). Arch resolves names
through systemd-resolved before `/etc/hosts`, so the session also bind-mounts an `nsswitch.conf` with
`hosts: files myhostname`. A killed session's CA is removed by the next one (`<prefix>/.powder-wrap-ca`).
After changing a game's config in Lutris, restart Lutris (it may rename the config file and keep a stale copy:
"This game has no executable set").

Why the CA goes into the prefix: a Wine/Proton prefix copies the host's root certificates into its registry
(`HKLM\Software\Microsoft\SystemCertificates\Root\Certificates`) when it is set up and crypt32 checks that copy,
so a CA added to the system later is not seen. Symptom: `tls ok` in the log, then the client closes without a
request (no access-log line). `admin trust install --wine-prefix <prefix> --wine <the prefix's wine>` does it
permanently; the wrapper does it per session.

**Live test, 2026-10-25 (Python server via powder-wrap):** Ubisoft Connect is fully online against Powder:
sign-in, friends, graphql, demux (ownership, cloud saves, friends), and it starts Steep (`product 3279 started`).
The game then stays at "loading, please wait 3/3": its own TLS check rejects the server (two handshakes from the
game right after start never complete; it sends no request). On Windows the launcher's TLS helper (`native/`,
injected at game start) handles this. **On Linux this is now solved too (2026-10-25): the version.dll proxy +
armer byte-patch make the game accept Powder's pinned certificate and go online** (see the native/ bullet
above). After closing, the wrapper's cleanup was verified: CA gone from the prefix, server stopped,
`/etc/hosts`, sysctl and the system CA store untouched.

Never run `admin trust install/remove` without `--wine-prefix` yourself (it changes the system trust store), and
don't run commands against the user's real prefix or data: test on scratch copies; the game is tested by the user.

## Dead ends (don't repeat)

- Access-finder (page-guard + VEH) on the player's position CRASHES the game: the page is hot (written by many
  threads per frame), the fault flood and single-step re-arm break. Make it safe (refuse hot pages, auto-disarm
  after a short burst) before using it again.
- Hardware/debug-register breakpoints: VMProtect detects/clears them.
- Both are replaced by Linux perf hardware watchpoints from outside the game (tools/trainer/watch.py, 2026-10-26):
  invisible to the game and to VMProtect. Wine holds the main thread's four debug slots (skipped).
- Bisect writes into stale snapshot addresses corrupt the game (the player got stuck floating, 2026-10-26):
  findauth2 now writes only live copies and undoes each nudge.
- Save migration from a live account: NOT a dead end after all (2026-10-26). The 32-hex at offset 8 is a
  per-session save-instance id; the account binding is a 16-byte XOR key per account over the body (aligned to its
  end). tools/savegame/savegame.py decodes saves (sections of AnvilNext LZO blocks) and re-keys them between
  accounts; no account id is inside. TRIED 2026-10-26: the game loads a re-keyed live save but shows a save
  mismatch / corruption popup ("Continue" = start over at the intro): it checks the save against the ONLINE stats
  (unlock bitfields, levels), which the save does not contain a copy of. A full migration needs the live stats too:
  `admin import <recordings>` from recordings of one's own online sessions (none on this device).
- MITM capture of real Ubisoft replies (tools/capture/): Connect's CEF ignores the prefix store and proxies; the
  game's OpenSSL is pinned. Real entitlement/offer formats can't be recorded that way.
- Imported stats (level) without a matching save make the game replay the intro ("progress, but at the start").
  SOLVED 2026-10-26: the "intro finished" state is the inventory item 8d9430c2-d08f-493e-8028-2fc2d448fdb0. The game
  looks up its offer right after the intro (and when it loads a save still holding the intro's completion record,
  which it then drops from the save); Ubisoft's store granted it, Powder had no offer, so every later save replayed
  the intro. The Rust server now grants it on that lookup (routes_ubi.rs ONBOARDING_TOKEN); not yet in Python.
  Confirmed three ways on 2026-10-26: a migrated live save (stats too small), 19.5 h playtime from that save, and
  setting all viewpoint bits (unlockable_exploration:2:*) by hand each brought the intro back. The game checks the
  save against the online stats; never change a player's unlock/level stats by hand (or restore an older save)
  without the other side matching. Recovery that worked: stats back to the event-derived values (stat_events
  folded with services/stats.rs's rules), the last good save restored locally AND in data/cloudsaves AND the
  cloudSave entity's slot sizes/hashes updated (else Connect warns "problem getting your latest save").

## State (2026-10-26)

End goal (README "Goal"): something like ReSkate for Skate 3 - a community-run Steep with a real, usable modding
API. Everything below is on `rework/rust-server`.

- **Online on Linux:** Connect + Steep online under Proton via powder-wrap (TLS helper: version.dll proxy +
  armer byte-patch). The Rust server is the default (POWDER_SERVER=python still works).
- **Server fixes found in play:** DLC worlds owned (demux product field 20 on "dlc" products; both servers;
  ULC keep the captured flag - force-owning cosmetics broke the store / heli tickets; HTTPS entitlements stay
  empty). "Intro finished" = inventory item 8d9430c2-... granted when the game looks up its offer (Rust only,
  routes_ubi.rs ONBOARDING_TOKEN). Without a pack, unknown reward items get synthetic free offers (the game
  ignores them; harmless). Watch for the 2026-10-25 hardening locking out real clients: "wrong signature" (see
  `nex_check_signatures`), CORS 401/403 for Connect's UI, 413 bodies, >1 MiB demux/NEX frames, >128 HTTPS
  connections from one address.
- **Overlay** (`native/overlay/`, POWDER_OVERLAY=1, Home): Server tab (admin API, Rust server), Debug tab,
  Trainer tab, Mods tab. Player position via 26 pointer chains in `steep_trainer.json` (7-11 agree after a
  restart). Noclip: the position (transform struct +0x50) is written as a 16-byte vector by steep.exe+0x124d0ae /
  +0x124d16b / +0x124d218 / +0x37fd6b (generic transform setters); caves skip only writes into the player's
  range; the jmp goes in via `armer.exe --write` (VMProtect blocks the game's own code writes). Fly = F6,
  camera-relative: camera.cpp calibrates the camera live (turn it once standing still) - pointer chains to it
  were NOT stable across launches. fixes.cpp: a null guard at steep.exe+0x5c8f73 (Alaska crash: no record for
  rider 0, gear key 6). crashlog.cpp logs faults with module+offset to powder_overlay.log.
  Camera-relative fly: CONFIRMED in-game (2026-10-26), but it had to be calibrated again too often. Since then
  the calibration waits for the rider to stand still instead of failing, and keeps every agreeing copy (a copy
  that stops following hands over to the next); this is NOT yet confirmed in-game.
- **Client-mod API** (`native/sdk/powder_api.h` v2, guide `native/sdk/README.md`): hooks (function, mid with
  registers), patch_code, find_pattern, read/write, add_panel (ImGui), on_frame, http to /_powder/ (no session),
  player_position. Hosted by the overlay (modhost.cpp, hooks.cpp on MinHook's trampolines, codepatch.cpp).
  Confirmed in-game with mods/asset_trace (ntdll file hooks).
- **Asset mods:** `mods/<name>/client/forge/<Base>/` -> powder-wrap builds a per-session `<Base>_patch_03.forge`
  (listed in `.powder-assets`, removed afterwards). CONFIRMED in-game: mods/model_swap (material swap) and
  mods/custom_texture (a generated "POWDER" checkerboard on the menu rider's jacket) and mods/mesh_edit (the
  jacket inflated 15 cm along its normals, all 4 LODs: it renders skinned and textured). Restart Lutris after
  changing POWDER_CLIENT_MODS in its environment. Game data in
  mods is generated locally by each mod's generate.sh and gitignored.
- **Blender path (2026-10-26, tools/forge/BLENDER.md):** `mesh.py export -o x.glb [--textures] [--rig ITEM]` /
  `import --glb x.glb [--to LOD] [--add] [--bone B]` (gltf.py, stdlib). Armature from the inverse bind matrices,
  bone names = CRC-32 (94 known: SkelAddon strings + Mixamo names; others `0x<hash>`; hierarchy guessed), real
  weights, bones the mesh lacks appended, rigid parts via Parent to Bone or `name@Bone`, static formats 6/8
  (handedness = position w sign, confirmed). Round trips exact (also through headless Blender 4.5.9/5.0.1/5.2.2:
  `blender --factory-startup -b --python s.py -- args`, or `uv pip install bpy` under Python 3.11). mods/shoulder_pads = new geometry with explicit
  weights, CONFIRMED in-game (2026-10-26: the pads render and follow the arms). `forge.py where <game> --id X` lists the datafiles
  holding a resource. Blender add-on `tools/forge/powder_blender.py` (sidebar tab Powder: Import item / Write mod,
  runs the tools in Blender's Python). The user's Blender 5.2.3 (Steam) has Blender Lab's MCP add-on; with the
  `blender` MCP server connected, Claude can drive it (execute_blender_code, screenshots).
- **mods/master_chief (2026-10-26):** a Sketchfab glTF (CC-BY, in gitignored `models/`) retargeted onto Steep's
  skeleton by make_chief.py (bone map, per-bone fit to the target's rest pose, weights renamed, back faces added).
  The first version replaced the default jacket: CONFIRMED in-game that added bones (38) and the fit work (issues:
  Mask1 R=255 turns dark cloth pale cream; single-sided shells were see-through). The user wants costumes, as
  additions if possible: a new item is not possible yet (TODO), so it replaces a costume: now the penguin
  (STP_M/F_Costume_005, in the store; --target urban_japan = 016, whose items are not in the store). Costume
  model per item: STP_M/F_Clothing records [class 0xb332698e, item tag u32] -> costume selector -> entity
  (README table). Store unlock CONFIRMED 2026-10-27: data/store_items.json (storemap.py from the running game)
  -> free shop offers; offers need public/partitionIndex/nameId/lastModified/dynamicItemsGroups or the game drops
  the connection (DE-010-05). The rider faces -Y (left = +X).
- **Tools:** tools/forge (forge.py archives + swap + where, texture.py, mesh.py + gltf.py, TEXTURES.md, MESHES.md,
  BLENDER.md),
  tools/savegame (save format, XOR key per account), tools/trainer (findpos/correlate/findauth2, watch.py and
  stackwatch.py perf watchpoints, ptrscan.py, dumpexe.py for the unpacked steep.exe, unwind.py, findcam.py,
  skiphook.py). Dump analysis: .pdata gives ~155k functions with bounds.

## Next steps (open)

1. Confirm in-game: Fly's camera stays found without new calibrations (camera.cpp's copy hand-over); then a
   mod that adds a bone / grows a bone map (export --rig).
2. New items instead of replacements: new ids (entity, template selector entry, bootstrap item, prefetch), e.g.
   Master Chief as a new costume (asked 2026-10-26; today it replaces costume 016). Game Bootstrap Settings
   (0x800) is one 146 MB datafile of 17,292 resources. The skeleton resource (hierarchy) is still not decoded.
3. Stuck "binoculars" viewpoint icons after today's restores: set exactly the missing bits (map viewpoints to
   unlockable_exploration:2:* bits, e.g. via the save's DropZoneFlags) - never mass-set stats (that brings the
   intro back). A server admin command "roll stats back to a time" (fold stat_events) would make repairs routine.
4. Admin teleport (asked 2026-10-25): a client+server mod pair using API v2 (on_frame + http); decide how the
   server identifies a player's game (the client carries no session).
5. Python parity for the intro token (and anything else marked "not yet in Python" in commit messages).
6. Windows: the launcher loading client mods through the overlay (it injects DLLs directly today);
   `native/build.ps1` overlay target.
7. Server management: DONE in Rust (2026-10-26, services/access.rs, migration 0006): player/address bans
   (temporary or not), whitelist (kv access/whitelist), kicks (SessionRegistry::kick closes NEX, websockets, demux);
   CLI `admin bans|ban|unban|ban-ip|unban-ip|kick|whitelist`, applied by the running server's 5 s sweep; admin page
   + api.admin.ban/kick/... Open: Python parity (the Python server ignores the rules; migration 0006 is shared),
   in-game test of a kick (does the game show a clean disconnect?).
8. Map / line builder mod (idea 2026-10-25, asked again 2026-10-26): Tier 1 curate/share Steep's own custom
   lines/challenges (UGC + entities already stored); Tier 2 an overlay planning tool drawing waypoints in 3D
   (needs the view matrix); Tier 3 new rideable geometry: static meshes (formats 6/8) now round-trip through
   Blender, so the steps are (a) replace a prop's mesh in-game (every placement changes), (b) decode the
   collision shapes (0xB22B3E61 `MeshShape`/`Merged Shape`; a visual mesh alone is not rideable), (c) decode the
   world cells (0xAC2BBF68 DataBlocks) to place new instances.
