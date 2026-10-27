# Powder — notes for Claude Code

Powder is a self-hosted stand-in for Steep's (Ubisoft) online services, so an owned copy stays playable, and a modding
platform for it (end goal: a community-run Steep like ReSkate for Skate 3). Work only on `main` (since 2026-10-27):
the Rust server (default) + mods + tools, the Python server (the reference for behaviour, `POWDER_SERVER=python`)
and the launchers. Remotes: `origin` = Toemmsen96/powder, `resteep` = resteepMod/rusty-powder (push to both).

**Read on demand, not up front:** `docs/NOTES.md` (port state, security review, dead ends, dated state and open
steps), README (users' docs), `tools/forge/*.md` (FORGE/ITEMS/WORLD/LOCALIZATION/MESHES/TEXTURES/BLENDER: game data),
`native/sdk/README.md` (client-mod API) and `native/sdk/CONTENT_MODS.md` (combining mods: the current plan),
`mods/<name>/README.md`. Check `git log --oneline -20` for what happened last.

## Build and test

```
cargo test && cargo clippy --all-targets   # all pass, no warnings
cargo build --release                      # powder-wrap/the launcher run target/release first: rebuild before the user tests
uv run pytest                              # Python; on Linux 4 Windows-only tests + test_limits' TLS test fail
native/build.sh [overlay|mods]             # MinGW: TLS helper, overlay, client mod DLLs
linux/powder-launcher [launch --dry-run]   # the Linux launcher (GUI without args)
```

Steep install (first PC): `G="$HOME/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game
Launcher/games/Steep"`; prefix `~/Games/ubisoft-connect` (GE-Proton via umu-run). Session log:
`$XDG_RUNTIME_DIR/powder-wrap.log`; game-side logs `$G/powder_{overlay,tls}.log`. Server data: `data/`
(powder.db, store_items.json, item_list.json). Scratch work goes in the session scratchpad, never in `$G` or the prefix.

## Conventions

- **Parity:** Rust ports follow Python byte for byte (`git show main:powder/...`, port the tests with their vectors);
  JSON via `util::dumps`; note behaviour differences in the commit message.
- **Style:** `//!` / docstring module docs saying what and why, few terse comments, ~120-column lines, `anyhow` with
  context, log targets `powder.<area>`.
- **Commits:** one focused commit per piece, body says what was tested, end with the Co-Authored-By line; push `main`
  to `origin` and `resteep` when done and green.
- **Server internals:** tokio + `Arc` services, `parking_lot` locks never across `.await`; `ModHost` behind a Mutex;
  HTTPS `handle()` runs in `spawn_blocking`.
- **Mods:** server = Lua (`mods/<name>/mod.lua`, sandboxed); client = `mods/<name>/client/` (DLLs need the player's
  approval, pinned to hashes: a server must never push code); asset mods = `client/forge/<Base>/` entries built into
  a per-session `<Base>_patch_03.forge`. Game data in mods is generated locally (gitignored).
- **Agents:** parallel work in worktrees; worktrees start from `main`; give
  disjoint files, commit but not push; cherry-pick here, test, remove the worktree, push.
- **Scratch server:** config with ports 18443/18080/16005/19000 in a scratch dir;
  `curl --resolve public-ubiservices.ubi.com:18443:127.0.0.1 --cacert <data>/tls/ca.pem https://...`.

## Safety (hard rules)

- Never run `admin trust install/remove` (system or prefix trust store) yourself.
- Never write into the game folder, the Wine prefix or the user's data except as the user asked (back up first);
  never start the game: the user tests in-game. Reading the install and the running game's memory is fine; writes
  into game memory only when the user is testing that and they are undone afterwards.
- The game checks saves against online stats: never change unlock/level stats by hand (brings the intro back).
- Once the game took a custom item list, keep serving one (`data/item_list.json`) or restore its cache backup
  (`build/backup_steep_tmp`), else it goes offline.

## Token-efficient working

- Prefer `grep -n`/`sed -n a,bp`/`tail` over reading whole files; read docs only for the task at hand.
- Filter command output (`| tail`, `| grep -v`), never dump logs; the session log is noisy (filter `INFO  https`).
- Delegate broad research/RE to one subagent with a precise brief; keep the main thread for decisions and merges.
- When a long command runs, don't poll: use background runs and wait for the notification.
