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

## The user's setup

Linux (Arch); Steep is installed through Lutris (Wine). Under Wine, Ubisoft Connect trusts the host's CA bundle
(that is why `trust.rs` has a Linux path), but an existing prefix only checks the copy of the roots it made
when it was set up (registry `HKLM\Software\Microsoft\SystemCertificates\Root\Certificates`): use
`admin trust install --wine-prefix <prefix> --wine <the prefix's wine>`. Symptom when missing: `tls ok`, then the
client closes without a request (no access-log line). Preferred on Linux: `linux/powder-wrap` (Lutris command prefix): a private user+mount+net namespace with its own
/etc/hosts, the server inside, the CA in the prefix only for the session (tested with umu-run/GE-Proton offline).
Still missing on Linux: injecting the game's TLS helper (`native/`) under Wine. The game itself can only be
tested by the user.
