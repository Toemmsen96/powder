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
cargo clippy --all-targets      # new code clippy-clean; 2 old warnings (gamedata/privacy.rs, nex/secure.rs) are known
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
Connect's `/v2/websocket/server`), NEX stats and matchmaking, channels, the importer, the admin CLI. Played
against the game once (2026-10-26, `POWDER_SERVER=rust powder-wrap`): Connect, demux, game start, NEX secure
channel (the game's packets carry our signature), cloud-save upload all worked; no unrouted game requests. Known gaps: `/_powder/status`
lists a game's address without its port (`NexChannel` does not know its UDP peer); entity/UGC/cloud-save files
are sent whole (no Range / If-Modified-Since); websocket permessage-deflate is declined. Each port's commit
message lists its behaviour differences. `run` logs a warning while the Rust server is untested; keep it accurate.

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
  accounts; no account id is inside. Not yet loaded by the game.
- MITM capture of real Ubisoft replies (tools/capture/): Connect's CEF ignores the prefix store and proxies; the
  game's OpenSSL is pinned. Real entitlement/offer formats can't be recorded that way.
- Imported stats (level) without a matching save make the game replay the intro ("progress, but at the start").

## Next steps

1. Done 2026-10-25: the game runs on Linux. The `native/` TLS helper (version.dll proxy + armer byte-patch)
   gets Steep past the loading screen and online under Wine/Proton (confirmed on the user's PC).
2. Play against the Rust server (`POWDER_SERVER=rust powder-wrap`) and fix what differs; then it can replace the
   Python server in the user's sessions. First session OK (2026-10-26). DLC worlds: the demux product list sets
   field 20 (owned) on the "dlc" products (Steep asks via UPLAY_USER_IsOwned); CONFIRMED in-game, in both
   servers (main e18e78f). The HTTPS entitlements endpoint stays empty (filling it did nothing). ULC keep the
   captured flag: force-owning cosmetics broke the store / heli tickets.
   Watch for the hardening (2026-10-25) locking out real clients: "wrong signature" in the log means the
   game fills PRUDP's signature field differently than our vectors (`[server] nex_check_signatures`: "log", the default, only logs; "enforce" refuses; see
   nex/connection.rs); also 401/403 "cors-refused" / "origin not allowed" for Connect's UI, 413 bodies, demux
   frames over 1 MiB, NEX messages over 1 MiB, more than 128 HTTPS connections from one address.
3. In-game UI from mods (asked for with the admin mod, 2026-10-25). DONE for Linux/Proton: `native/overlay/`
   is a Dear ImGui overlay (D3D11 Present hook via MinHook; the version.dll proxy LoadLibrary's it when
   powder-wrap runs with POWDER_OVERLAY=1). Toggle HOME. Server tab = the admin actions over
   `/_powder/admin/api/*` (needs POWDER_SERVER=rust); Debug tab = status, the broadcast feed (new `feed`
   action in mods/admin/mod.lua), FPS, HTTP log, and a generic in-process memory scanner. Build with
   `native/build.sh overlay`; `native/bin/nettest.exe` checks the networking without the game. Verified
   headless: DLL loads, Present hooked, ImGui init, auto-login + players fetched. Trainer tab (2026-10-25):
   a generic chassis for trainer features - address slots with freeze, set, X/Y/Z assignment, Fly (hold
   position, move with W/A/S/D + Shift/Ctrl) and Teleport (save/go-to spot).
   WORKING 2026-10-25: character fly/teleport confirmed in-game. The player world position was found by
   external memory analysis (tools/trainer/: findpos -> correlate (movement-delta) -> findauth2 (bisect with
   held-out observers, gentle single writes); /proc/<pid>/mem read+write works under Proton, no ptrace). Writing
   the authoritative X/Y/Z moves the real character. Addresses are per-launch.
   Access-finder (2026-10-25, native/overlay/access.{h,cpp}): the overlay's Trainer tab can PAGE_GUARD a watched
   address + VEH to capture the instruction(s) that read/write it, decode the operand with HDE64 (MinHook's
   hde64) to recover the struct base register + value + field offset, show them (module+offset), and NOP a
   writer to freeze/override that field. VMProtect guards its own code pages, not this data page, so the guard
   works in-process. This is the bridge to a stable pointer path (so the position needn't be re-hunted each
   launch) and to the hook manager / client-mod hook API (next).
   NOCLIP WORKING 2026-10-26 (via tools/trainer/skiphook.py; the overlay port is untested in-game): the player's
   position is a float3 at transform struct +0x50, written as a 16-byte vector by steep.exe+0x124d0ae /
   +0x124d16b / +0x124d218 (movaps [reg+0x50]) and +0x37fd6b ([rdx+0x30], rdx = struct+0x20) - generic
   transform writers (hundreds of objects), so the hook skips only writes into the player's range. Writing the
   position while the hooks skip the game's writes = noclip; off = physics resumes from there (Z is up).
   native/overlay/noclip.cpp does it in-process (caves near steep.exe in our memory, WriteProcessMemory on
   ourselves patches the 5-byte jmp - works under Wine on read-only code), driven by
   native/overlay/steep_trainer.json (deployed as powder_trainer.json): the writers + their bytes, and 26
   pointer chains to the struct from one session (tools/trainer/ptrscan.py) - not yet confirmed across launches;
   the overlay needs two chains to agree. Trainer tab: Find player (auto), Fly = F6, noclip while flying,
   re-resolves the position every frame while flying (map change). CONFIRMED 2026-10-26: chains found the
   player after a restart (7 of 26 agree); hooks install in-game via armer.exe --write (VMProtect blocks the
   game's own writes to its code, also WriteProcessMemory on itself).
   MOD API (2026-10-26, Wine-tested, not yet in-game): native/sdk/powder_api.h is the C API client mods get via
   `powder_mod_init(api, mod)`: log, module_base, find_pattern, read/write, patch_code, hook (function detour,
   MinHook's trampoline builder + our write_code), hook_mid (register-context callback: PowderRegs with the GPRs
   and xmm0-5, changes are kept), unhook, add_panel (+ the overlay's ImGui context), player_position. The overlay
   is the host (modhost.cpp loads powder_mods/*.dll, Mods tab; hooks.cpp; codepatch.cpp = code writes with the
   armer fallback). Mods: mods/<name>/src/*.cpp -> native/build.sh mods -> mods/<name>/client/<name>.dll
   (gitignored); powder-wrap loads only those named in POWDER_CLIENT_MODS. Example: mods/asset_trace (hooks
   ntdll NtCreateFile/NtOpenFile/NtReadFile: per-file read stats in the Mods tab, optional read log
   powder_asset_trace.log = "<ms> <file> <offset> <length>"). Steep's data: AnvilNext .forge archives
   ("scimitar", v27) + _patch_01/_02 per archive, Wwise .pck, Bink .bk2, terrain .tbf (tools/forge/ for the
   format). Deferred: camera-relative fly (needs the view matrix); the Windows launcher loading mods through the
   overlay (it injects client DLLs directly today); live POWDER_* delivery to non-admin players'
   overlays (needs a websocket client + player-ticket auth); the Windows launcher path (ship the DLL as
   `mods/admin/client/` + `mod.toml [client] inject` so `inject_mods` loads it with per-player approval);
   `native/build.ps1` overlay target for Windows builds.
4. Done 2026-10-25: Connect's `/v2/websocket/server` is routed (Python on main, and the Rust port).
5. Done: Linux launcher GUI, `linux/launcher/` (GTK4 + Libadwaita GUI + CLI) and the `linux/powder-launcher`
   wrapper.
6. Server management interface (asked for 2026-10-25): access control for a Powder server - whitelist /
   blacklist mode, add/remove players, ban/kick - enforced in the accounts/sign-in + NEX/HTTPS auth paths,
   with admin CLI commands and an entry in the admin mod (web + the new overlay). Decide storage (config vs
   DB) and enforcement points with the user.
7. Map / line builder mod (idea raised 2026-10-25). Feasibility (checked against routes_ugc.rs / routes_ubi.rs):
   TIER 1 (feasible, mostly plumbed) - Steep creates custom lines/challenges, trails and challenge ghosts and
   Powder already stores/serves them (UGC service + profile entities); a mod would curate/publish/browse/share
   them (web + overlay UI). Next: a read-only RE pass to pin down the entity/UGC types + metadata Steep writes
   for a custom challenge, then mod + endpoints. TIER 2 (modest RE) - an overlay "ghost line"/planning tool:
   RE the player + camera world transform (the overlay's memory scanner is the groundwork), draw planned
   waypoints/gates projected into the 3D view, save to Powder, show on others' overlays; visual only, no
   collision. TIER 3 (shelved, not realistically feasible) - spawning real rideable ramps as new world
   geometry: closed VMProtect'd proprietary engine, no SDK; scene/actor spawn + collision + physics RE is
   research-grade and fragile. Recommendation: Tier 1, then Tier 2; skip Tier 3.
