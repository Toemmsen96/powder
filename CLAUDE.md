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
| HTTPS pipeline + router | `https/mod.rs`, `https/routes_ubi.rs`, `https/routes_powder.rs` | `transport/https/app.py`, part of `routes_ubi.py` |
| built-in replies (no pack) | `builtin/` | `builtin/` |
| NEX transport + handlers | `nex/{connection,server,dispatch}.rs`, `nex/handlers/` | `transport/nex/` (login, gamestorage, profile) |
| Storm (UDP P2P helpers) | `storm.rs` | `transport/storm.py` |
| accounts, sessions | `core/`, `services/accounts.rs`, `services/seed.rs` | `core/`, `services/accounts.py` |
| services | `services/{economy,entities,stats,leaderboards,cloudsaves,friends}.rs` | `services/*.py` |
| Ubisoft Connect's demux | `demux/` (shares `CloudSaveService` and `ConnectClients` via `DemuxDeps`) | `transport/demux/` |
| mods | `mods.rs` (Lua), `client_mods.rs` | `plugins/` (new design) |

CLI: `run` (default), `mods`, `admin trust|players|accounts|account-*`, `gamedata build|verify|zip|install`.

Python tests (`tests/`, pytest; `uv run pytest`): on Linux 4 fail before and after any change
(`test_launcher.py` run_game tests and `test_packaging.py::test_busy_port_is_detected` need Windows). The launcher's
client mods are `powder/launcher/mods.py` (+ the "Client mods" step in `session.py`, `tests/test_client_mods.py`).

### Not ported yet (pick from here)

- HTTPS: `routes_profile.py` (users/me, profiles, friends, graphql…), `routes_ugc.py`, the rest of
  `routes_powder.py` (status, account API, ca.pem), `websocket.py`, and the rest of `routes_ubi.py` (inventory
  + store → `services/economy.rs` is ready, entities v2 → `services/entities.rs` is ready, Connect cloud saves).
- NEX handlers: `stats.py` (→ `services/stats.rs` is ready), `matchmaking.py` (needs `services/matchmaking.py`).
- Services: `matchmaking.py`, `channels.py`, `importer.py`. The cloud-save HTTPS routes must use the same
  `CloudSaveService` as the demux (built in `run`); `/_powder/status` lists `ConnectClients::snapshot()`.
- Admin: inventory, grant, stats, ledger, events, cloud-saves, scores, achievements, entities, import, reset, offers.
- `run` logs a warning listing what is missing; keep it accurate.

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
injected at game start) handles this; on Linux nothing does yet. After closing, the wrapper's cleanup was
verified: CA gone from the prefix, server stopped, `/etc/hosts`, sysctl and the system CA store untouched.

Never run `admin trust install/remove` without `--wine-prefix` yourself (it changes the system trust store), and
don't run commands against the user's real prefix or data: test on scratch copies; the game is tested by the user.

## Next steps

1. Connect's notification websocket: it asks for `GET /v2/websocket/server?SpaceIds=...&NotificationTypes=...`,
   which the Python server does not route (`/v2/websocket` only) -> UNMATCHED, retried every ~30 s. Fix in the
   Python server (main) and when porting `websocket.py` to Rust.
2. The game on Linux: the TLS helper from `native/` is Windows-only so far; getting the game past the loading
   screen under Wine/Proton is the open problem (the user decides how to approach it).
3. The Rust port: see "Not ported yet" above; once the HTTPS routes, NEX stats/matchmaking and the websocket are
   done, `POWDER_SERVER=rust powder-wrap` can replace the Python server in the user's sessions.
