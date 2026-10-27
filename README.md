# Powder

**Keep Steep playable after Ubisoft's servers are gone.** Powder is a self-hosted stand-in for the online
services Steep needs, plus a launcher that sets a PC up to use it. It preserves _your own_ copy of the game:
progression, store, challenges and rewards all keep working without Ubisoft. On top of that it is growing a
modding system for new content and gameplay changes.

> [!WARNING]
> Powder is an **unofficial fan project**, not affiliated with or endorsed by Ubisoft. Using it may breach the
> terms of service of Ubisoft, Steam or others, and could lead to account bans or suspensions. You use it **at your
> own risk**; the authors and contributors are not responsible for any consequences. You must own the game
> legitimately. See [Disclaimer](#disclaimer).

## At a glance

| | |
| --- | --- |
| **Start here** | [Disclaimer](#disclaimer) · [What it does](#what-it-does) · [Game features](#game-features) |
| **Play** | [Install (Windows)](#install-windows) · [Linux (Lutris, Proton)](#linux-lutris-proton) · [Run from source](#run-from-source) |
| **Host a server** | [Players and accounts](#players-and-accounts) · [Server management](#server-management) · [Game-data pack](#game-data-pack) |
| **How it works** | [Launcher](#launcher) · [Client mods](#client-mods) · [Ubisoft Connect](#ubisoft-connect) · [Rust server](#rust-server) |
| **Extras** | [In-game overlay and trainer](#in-game-overlay-and-trainer) · [Modding](#modding) |
| **Reference** | [Files](#files) · [Ports](#ports) · [Progression](#progression) · [Internals](#internals) · [Licence](#licence) |

## Disclaimer

- Powder is an unofficial fan preservation project. It is not affiliated with, endorsed by or supported by Ubisoft
  or Valve. Steep and Ubisoft Connect are trademarks of Ubisoft.
- You need your own, legitimately bought copy of Steep, from Steam or from Ubisoft Connect. The repository
  contains no game files and no data from Ubisoft's servers; asset mods generate what they need from your own
  install.
- Running a replacement server, redirecting Ubisoft's hostnames, attaching code to the game or modding it may
  breach the terms of service or end-user licence agreements of Ubisoft, Steam or other parties. Whether it does,
  and what follows from that, depends on those terms and on where you live; this README makes no legal claim
  either way.
- The authors and contributors are **not responsible** for any consequences of using Powder, including terms of
  service breaches, bans or suspensions of Ubisoft, Steam or other accounts, lost or corrupted progress or save
  files, data loss, or damage to your system. Back up what matters to you.
- Powder comes with **no warranty**, to the extent permitted by law: it is free software under the GNU Affero
  General Public License v3.0 or later ([LICENSE](LICENSE), sections 15 and 16). Use it at your own risk.

## Goal

Keeping Steep online is the first half. The end goal is something like ReSkate (the community revival of
Skate 3): a community-run Steep that lives on, with a **real, usable modding API** - mods that add content
(models, textures, gear, lines), change gameplay through game hooks, and talk to their server side, and that
players enable deliberately (a server can never push code to a player). Where that stands today:
[Modding](#modding).

## What it does

| Part | Where | What |
| --- | --- | --- |
| **Server** (`powder-server`) | `server/` (Rust, the default), `powder/server/` (Python) | One process that answers the game and Ubisoft Connect in place of Ubisoft: the UbiServices HTTPS API (all `*.ubi.com` hosts), the Quazal NEX/RendezVous server, Ubisoft Connect's demux protocol and the Storm peer-to-peer helpers. Player state lives in SQLite. Both servers read the same config and data: [Rust server](#rust-server) |
| **Launcher** (`powder`) | `powder/launcher/` (Windows) | Runs on the PC with the game: checks and sets up what is needed there (server, DNS, certificate, Ubisoft Connect), starts Steep online against the server, and can undo all of it: [Launcher](#launcher) |
| **Linux wrapper and launcher** | `linux/` | Runs Ubisoft Connect and Steep against Powder for one session under Wine/Proton (e.g. Lutris), changing nothing on the system: [Linux](#linux-lutris-proton) |
| **TLS helper** | `native/` | Makes the game accept the Powder server's certificate, and only that one |
| **In-game overlay and trainer** | `native/overlay/` | A Dear ImGui overlay inside Steep: server admin, debug tools, Fly/noclip, and the host for client mods: [Overlay](#in-game-overlay-and-trainer) |
| **Mods** | `mods/`, `native/sdk/`, `tools/forge/` | Server mods (Lua), client mods (DLLs) and asset mods (`.forge` entries, models from Blender): [Modding](#modding) |

## Game features

What Steep needs a server for, and how far Powder covers it. In short: sign-in, progression, the store, challenge
rewards, leaderboards, replays and ghosts, player-made challenges, cloud saves and multiplayer on a local network
all work; the store and rewards need the optional [game-data pack](#game-data-pack). Friends are partly emulated;
Ubisoft Club, news and Connect's store pages are not planned.

<details>
<summary><b>Full feature table</b></summary>

| Status         | Meaning                                                                                                                 |
| -------------- | ----------------------------------------------------------------------------------------------------------------------- |
| ✅ Working     | Works like with Ubisoft's servers                                                                                       |
| 🟡 Partly      | The game accepts the answer, but the feature is limited, not verified everywhere, or not usable without technical steps |
| ❌ Not working | The game asks, the server answers with an empty or placeholder reply: the feature is missing                            |
| ⛔ Not planned | Not part of Steep's gameplay, or not worth emulating                                                                    |

**Getting online**

| Feature                                         | Status | Notes                                                                                       |
| ----------------------------------------------- | ------ | ------------------------------------------------------------------------------------------- |
| Ubisoft Connect sign-in                         | ✅     | With your Powder account (registered in the launcher); Connect stays signed in              |
| Game login (Steam and Ubisoft Connect versions) | ✅     | Both versions of the final update (TU7)                                                     |
| Owned games / DLC ownership                     | ✅     | Steep and its add-ons, built in; the DLC worlds are owned                                   |
| Game configuration, season info                 | ✅     | Built in (the game-data pack's copy is used when installed)                                 |
| Several players on one server                   | ✅     | An account per player (email and password), each with their own progress                    |

**Progression and economy**

| Feature                                          | Status | Notes                                                                                                                                  |
| ------------------------------------------------ | ------ | -------------------------------------------------------------------------------------------------------------------------------------- |
| Level and XP per sport, playtime                 | ✅     | From the game's stat uploads; never goes down                                                                                          |
| Medals, dropzones, memorables, unlocks           | ✅     |                                                                                                                                        |
| Store: buying items with credits                 | ✅     | Needs the game-data pack (the catalogue); without it the store is empty                                                                |
| Challenge rewards (credits, items)               | ✅     | Needs the game-data pack (rewards are store offers)                                                                                    |
| Rider card and favorites                         | ✅     |                                                                                                                                        |
| Personal best per challenge                      | ✅     | Stored on the server                                                                                                                   |
| Achievements                                     | ✅     | Stored per player; names and icons in Ubisoft Connect need the game-data pack                                                          |
| Taking over your progress from Ubisoft's servers | 🟡     | Only from recordings of your own online sessions, via the command line (`admin import`); players can't make such recordings themselves |

**Competing and sharing**

| Feature                           | Status | Notes                                                                       |
| --------------------------------- | ------ | --------------------------------------------------------------------------- |
| Leaderboards (challenge rankings) | ✅     | The server's players, by their best score; weekly and daily boards show the bests set in that period |
| Replays / ghosts of other players | ✅     | Saved and shared rides, photos (likes, deleting) are kept and seen by the other players. Ghosts: only ranked challenges have them (as on Ubisoft's servers); a ranked run that sets a personal best or a record uploads its ghost, and the others race the record holder's |
| Player-made trails / challenges   | ✅     | Saved rides and challenges made from a replay are kept, with a leaderboard per challenge; the others download them from your profile in the social tab (one world at a time). Tested with two accounts on one PC. Ubisoft's own featured trails ("Specials", the last ones ended in 2019) are not served |
| Seasons and tournaments           | 🟡     | No season running, as on Ubisoft's servers at the end; nothing changes      |

**Playing with others**

| Feature                                                  | Status | Notes                                                                     |
| -------------------------------------------------------- | ------ | ------------------------------------------------------------------------- |
| Multiplayer sessions (other riders in the world, groups) | ✅     | On a local network (tested with two PCs): riders in the same area meet; groups by pressing G next to each other or "Join group" in the social tab; spectating |
| Matchmaking                                              | ✅     | Players in the same world and area (cell) are put into one session; no matchmaking map, the game splits areas itself |
| Friends list, invites, blocks                            | 🟡     | Every other player of the server is a friend (online when their game or Connect is connected). Invites are refused: accepting needs Ubisoft Connect's own pages; no blocks |
| Other players' profiles (rider card, level)              | ✅     | Steep's profile view of a friend                                          |

**Ubisoft services around the game**

| Feature                                                           | Status | Notes                                                                                   |
| ----------------------------------------------------------------- | ------ | --------------------------------------------------------------------------------------- |
| Ubisoft Connect window and in-game overlay (Shift+F2)             | 🟡     | With the game-data pack: Ubisoft's pages; without it: a Powder page of its own          |
| Ubisoft Club (actions, units, rewards)                            | ⛔     | Ubisoft's cross-game reward system: the action list is shown, no progress or rewards    |
| Cloud saves                                                       | ✅     | The game's own save files follow your account to another PC, also between the Steam and the Ubisoft Connect version |
| News, recommendations, Ubisoft Connect store and free-games pages | ⛔     | Ubisoft's own content, not part of Steep                                                |
| Telemetry, crash reports                                          | ⛔     | Acknowledged and dropped (the game's error reports are kept in the server's log folder) |

</details>

## Install (Windows)

You need Windows 10/11 with Ubisoft Connect, Steep's final update (TU7) from Steam or Ubisoft Connect.
A game-data pack (`.zip`, see [Game-data pack](#game-data-pack)) is optional: it adds the store and challenge
rewards.

1. Download `Powder-Setup-vX.Y.Z.exe` from the [Releases](../../releases) page and run it. It installs for
   your Windows user only (no admin rights). While Powder is not code-signed, Windows SmartScreen warns
   about an unknown app: **More info → Run anyway**.
2. Start **Powder** and press **Play**. It walks you through what it needs to set up (every Windows
   prompt is explained before it appears). If you have a game-data pack, add it once with **Choose
   pack…**; a checkbox then switches it on and off.

Powder keeps your progress, the game-data pack and its settings in `%LOCALAPPDATA%\Powder`, apart from
the program, so updating (install the new version over the old one) never touches them. Uninstalling
(Windows settings → Apps) first opens Restore system to undo what Powder set up on this PC, then asks
whether to keep your data.

Commands for the installed program: `powder-cli.exe` in the install folder
(`%LOCALAPPDATA%\Programs\Powder`), e.g. `powder-cli server admin players`. The installed launcher runs the
Python server.

## Linux (Lutris, Proton)

`linux/powder-wrap` runs Ubisoft Connect and Steep against Powder for one session and changes nothing on
the system. With it Ubisoft Connect **and** Steep are fully online under Proton.

**Easiest: the Linux launcher.** Install Ubisoft Connect once (e.g. with Lutris; Steep in it), build the native
parts with `native/build.sh` and the Rust server with `cargo build --release`, then run `linux/powder-launcher`
(`linux/powder-launcher install-desktop` puts it into the application menu). It finds the Wine prefix, the Proton
build (the prefix's `version` file) and `umu-run` by itself, starts Ubisoft Connect and Steep through powder-wrap,
and its **Mods** page switches the mods for the game on and off (with a Build/Generate button for mods that need
it) - no environment variables, no Lutris settings. Settings already in Lutris' environment are taken over once.
Details: [Linux launcher](#linux-launcher).

**Or in Lutris:** build the native parts once with `native/build.sh` (TLS helper, overlay, client mods;
needs a MinGW-w64 cross compiler) and the Rust server with `cargo build --release`. Then Configure the game
(Steep, and Ubisoft Connect if you start it on its own) > System options (show advanced options) > **Command
prefix**: `/path/to/powder/linux/powder-wrap`. Close Ubisoft Connect first, and don't run another Powder server
on the same `config.toml`. After changing a game's environment in Lutris, restart Lutris.

By hand: `WINEPREFIX=~/Games/ubisoft-connect linux/powder-wrap <command>`.

<details>
<summary><b>What powder-wrap does, and its settings</b></summary>

The session gets its own network (loopback only: the game and Connect have no network route to Ubisoft or
anywhere else; it is not a sandbox, Unix sockets on the filesystem such as D-Bus or X11 stay reachable), the
Ubisoft names point at the server through a private `/etc/hosts`, the Powder server runs inside it, and the
local CA is trusted by the Wine prefix only while the session lasts. No hosts entries, no system CA, no
`sysctl`, no root.

Steep itself needs its certificate check intercepted; on Windows the launcher injects the helper, on Linux
powder-wrap deploys a version.dll proxy of it into the game (`native/version_proxy.c` + `armer.exe`, built by
`native/build.sh`), which accepts only the Powder server's own certificate.

Settings (environment):

| Variable | What |
| --- | --- |
| `POWDER_CONFIG` | the server's config (default: `config.toml` in the repository) |
| `POWDER_SERVER` | `rust` or `python` (default `rust`; the Python server when no Rust build exists) |
| `POWDER_WINE` | the wine the prefix runs with |
| `POWDER_OVERLAY=1` | the [in-game overlay](#in-game-overlay-and-trainer) |
| `POWDER_CLIENT_MODS=name,name` | the client mods of this repository to load for the session ([Modding](#modding)) |

The server's output goes to `$XDG_RUNTIME_DIR/powder-wrap.log` (without `XDG_RUNTIME_DIR`:
`~/.cache/powder/powder-wrap.log`).

</details>

### Linux launcher

`linux/powder-launcher` is a small GTK4/Libadwaita app that drives all of this from a window instead of
environment variables: it finds the game (Wine prefix with Ubisoft Connect from Lutris' config or `~/Games`, the
Proton named in the prefix's `version` file, `umu-run`) and starts it, picks the server implementation (`python`
or `rust`), toggles the in-game overlay, chooses the **mods for the game** (overlay DLLs, asset mods, session
steps; with their status, a Build/Generate button, and a warning when two change the same entries) and the
server mods (a game mod's server half follows it), manages your list of Powder servers (a name and a Join
address `host#<fingerprint>`; joining another server is not done yet), and launches Steep - through
`linux/powder-wrap`, so everything above still holds. Its server config is `config.launcher.toml`, your
`config.toml` with the launcher's mod choice.
Run it with no arguments for the GUI:

```
linux/powder-launcher
```

<details>
<summary><b>Command line, requirements, desktop entry</b></summary>

The same wrapper is also a command line for the parts you want to script (`detect`, `list-client-mods`,
`enable-client-mod NAME`, `launch [--dry-run]`, `list-mods`, `install-desktop`, and so on). It runs the Python package in `linux/launcher` with the repo's
`.venv` if there is one, else `uv`, else the system `python3`; the GUI needs PyGObject with GTK 4 and
Libadwaita installed (on Arch: `python-gobject gtk4 libadwaita`). `linux/powder-launcher.desktop` is a desktop
entry for a menu or launcher - edit its `Exec=` to the absolute path of `linux/powder-launcher`. The overlay
and its admin Server tab need the Rust server (`POWDER_SERVER=rust`).

</details>

<details>
<summary><b>Trusting the local CA in a Wine prefix permanently</b></summary>

A Wine/Proton prefix (Lutris) copies the system's root certificates when it is set up and checks against that
copy, so a CA added to the system later is not seen. powder-wrap trusts the CA in the prefix for each session;
to do it permanently, add the local CA to the prefix itself, with the wine the prefix runs with (Ubisoft
Connect closed, restart it afterwards):

```
powder-server admin trust install --wine-prefix ~/Games/ubisoft-connect \
    --wine ~/.local/share/Steam/compatibilitytools.d/GE-Proton11-11-x86_64/files/bin/wine
```

Without `--wine-prefix` it goes into the system store (`trust anchor`, as root), which prefixes made later
copy.

</details>

## Run from source

**Python** (the launcher and the Python server): needs [uv](https://docs.astral.sh/uv/) (it brings Python 3.12)
and [zig](https://ziglang.org/) to build the TLS helper. Files go to the current folder instead of
`%LOCALAPPDATA%\Powder`.

```
native\build.ps1                                    # once: builds the TLS helper
uv run powder-server gamedata build <recordings> gamedata   # optional: the game-data pack (see below)
uv run powder                                       # the launcher: press Play
```

Other commands:

```
uv run powder-server run        # the server alone (uses ./config.toml, see config.example.toml)
uv run powder --status          # what the launcher has set up on this PC
uv run powder --restore         # undo it
uv run pytest
```

**Rust** (the server): builds with the pinned toolchain (`rust-toolchain.toml`: Rust 1.99.1, edition 2024).

```
cargo build --release           # target/release/powder-server
powder-server run               # same config.toml and data/ as the Python server
cargo test
```

On Linux, `native/build.sh` builds the TLS helper, the overlay and the client mods (MinGW-w64), and each asset
mod's `mods/<name>/generate.sh` makes its entries from your own Steep install.

Both servers take the same commands. Below they are written as `uv run powder-server ...` (Python) or
`powder-server ...` (the Rust build); the installed Windows program takes them as `powder-cli.exe server ...`.

## Game-data pack

Powder runs without one: the server builds what the game and Ubisoft Connect need to go online
(`powder/server/builtin/`: Steep's owned-games entries, the SDK configuration, the game's start replies;
only ids, hostnames, URL templates and values written for Powder). What it cannot build is Ubisoft's
game data: the store catalogue (2,000+ offers, which also define challenge rewards), Ubisoft Club actions,
the achievement names and icons Connect shows, and which items each DLC grants. That data is not in this
repository. It comes as an optional pack in `gamedata/`, built from recordings of your own online
sessions; when installed, its replies take precedence over the built-in ones:

```
uv run powder-server gamedata build <recordings> gamedata    # allow-list + privacy check
uv run powder-server gamedata verify                         # manifest + file hashes
uv run powder-server gamedata zip steep-gamedata.zip         # the configured pack -> one .zip
uv run powder-server gamedata install steep-gamedata.zip     # .zip or folder -> the configured pack
```

The pack travels as one `.zip`: the launcher installs it with **Choose pack…** (checking format and file
hashes before using it), and `gamedata install` does the same from a console.

<details>
<summary><b>Privacy and safety checks, Connect's pages</b></summary>

The builder copies only allow-listed, game-wide replies. Before writing the pack it collects every
identifying value from the recordings (account and profile ids, names, email, tickets, friends, other
players) and refuses to write the pack if any of them appears in it (names down to 3 characters, as whole
words; inside zip and gzip bodies too). Everything personal (profile, friends, login, owned games beyond
Steep) is generated by the server instead of replayed. Zipping runs a privacy check that needs no recordings
(identifying keys, e-mail addresses).

A pack may come from someone else, so the server trusts none of it: on install and on every start it applies
the builder's allow-lists again (only JSON replies for the allow-listed paths and hosts, plus Connect's
achievement definitions; size-capped zips) and refuses a pack that does not pass; Connect's owned-games
entries are always the server's own (a pack only says which of Steep's products to list), so a pack cannot
change what Connect starts.

Ubisoft Connect's window and in-game overlay are a frame from Ubisoft's CDN that loads its pages from the
addresses its settings list. With a pack, those are Ubisoft's pages; without one, the server lists a
single page of its own (`/_powder/overlay/powder.js`), so Connect shows a Powder page instead of its
library and store. Settings Connect still holds in its browser cache are never overwritten (the server
answers "not modified"). If Connect has never been online on a PC and fails with dolphin-032, start it
once normally (without Powder: **Restore system**, sign in to your real account, then Play again).

</details>

## Players and accounts

Every player has an account on the server: an email and a password, registered in the launcher (the
**Account** row or the Account step of Play) and used to sign in to Ubisoft Connect. The email can be
changed later; the password is stored only as a scrypt hash, never logged. Wrong passwords are
rate-limited per address. If someone forgets their password, the host sets a new one:

```
uv run powder-server admin accounts                    # email -> player
uv run powder-server admin account-create <email> <name>   # asks for the password
uv run powder-server admin account-password <email>    # asks for the new password
uv run powder-server admin account-email <email> <new email>
uv run powder-server admin account-link <email> <player id>   # the account owns that player instead
```

<details>
<summary><b>Who becomes which player, registration, sessions</b></summary>

The first account on a server takes over the player already there (e.g. your imported progress) if it is
registered on the server's own PC (or made with `admin account-create`); every further account, and one
registered from another PC, starts like a new Ubisoft account: level 1, empty inventory, `starting_credits`.
A server without any account accepts every sign-in, as before accounts existed, until the first player
registers (a `[server] public = true` server never does). Once accounts exist, every game and Connect call
that acts for a player needs that player's session, and a player changes only its own inventory, rider card,
saves and photos. `[players] registration = "closed"`: only the host creates accounts.

</details>

To continue with your own progress, import it once from your recordings (then register: the first account
takes it over):

```
uv run powder-server admin import <recordings>      # level, credits, items, stats, rider card, achievements
uv run powder-server admin import <recordings> --replace
```

After the import the server never reads the recordings again. Recordings contain personal data (your
account, friends, other players on leaderboards), so `recordings/` is gitignored and nothing in the
repository depends on it.

## Server management

All commands work on the default player unless a player is given.

<details>
<summary><b>Admin commands (players, inventory, stats, ...)</b></summary>

```
uv run powder-server admin players
uv run powder-server admin inventory              # item count + credits
uv run powder-server admin grant credits 50000    # or an item id; negative to remove
uv run powder-server admin stats --key level
uv run powder-server admin ledger                 # recent inventory changes
uv run powder-server admin offers armada          # search the store catalogue
uv run powder-server admin events                 # recent progress uploads from the game
uv run powder-server admin scores                 # personal best per challenge
uv run powder-server admin entities               # rider card, favorites
uv run powder-server admin achievements --all     # earned achievements (and the open ones)
uv run powder-server admin cloud-saves [PLAYER] --delete   # remove a player's cloud save
uv run powder-server admin import <recordings>    # your own recorded progress, once
uv run powder-server admin reset                  # the player starts again as a new player
uv run powder-server admin trust status           # the local CA in Windows (also install / remove)
```

</details>

### Bans, whitelist and kicks

The Rust server can keep players out: bans of players (by profile id, account email or player name), bans of
addresses or ranges, a whitelist that can be switched on and off, and kicks. Bans may be temporary. The rules
are stored in the server's database, so the CLI and the admin page change the same list, and a running server
applies changes made with the CLI within about 5 seconds (kicking whoever is no longer allowed).

```
powder-server admin bans                                       # the bans and the whitelist
powder-server admin ban PLAYER [--for 7d] [--reason TEXT]      # 30m, 12h, 7d, 2w; default: until unbanned
powder-server admin unban PLAYER
powder-server admin ban-ip ADDRESS[/PREFIX] [--for 7d] [--reason TEXT]   # 192.0.2.7, 192.0.2.0/24, 2001:db8::/32
powder-server admin unban-ip ADDRESS                           # the same address or range
powder-server admin kick PLAYER [--reason TEXT]                # they may sign in again
powder-server admin whitelist on|off|add PLAYER|remove PLAYER|list
```

- A banned player, or anyone from a banned address, can't sign in, register, or use a ticket they already have.
- While the whitelist is on, only its players and the server's admins get in.
- A kick ends the player's sessions and closes their game's, websockets' and Ubisoft Connect's connections;
  unless they are banned, they can sign in again at once.
- The server's admins (`[server] admins` in `config.toml`) are exempt from the whitelist and address bans, and
  can't be banned, so a host can't lock themselves out of the admin page.
- Only the Rust server enforces these rules; the Python server ignores them.

### Admin page

The `mods/admin` server mod is a web page for the server's admins at `https://<server>/_powder/admin` (Rust
server): players and who is online, inventory, credits and items, stats, resetting a player, messages to everyone
online, and bans, the whitelist and kicks. List the admins' profile ids (`powder-server admin accounts` shows
them) in `config.toml` and sign in on the page with that account's email and password:

```toml
[server]
admins = ["<profile id>"]
```

Without admins configured the mod does nothing. From the server's PC the page is at
`https://127.0.0.1:<https_port>/_powder/admin` (the browser does not know the server's local CA: accept the
warning, or import `data/tls/ca.pem`). Under `linux/powder-wrap` the server lives in the session's own network:
run it outside the wrapper to use the page. In-game, the overlay's **Server** tab offers the admin actions
(players, credits/items, stats, broadcasts).

## Launcher

`powder` (settings in `launcher.toml`, see `launcher.example.toml`; all optional) works with both
versions of Steep. It finds them wherever they are installed, from what Steam (its libraries and app
manifests) and Ubisoft Connect (its install records) note about them. With both installed it shows
**Play (Steam)** and **Play (Ubisoft Connect)**, otherwise a single **Play**.

Where the server is, is chosen in the **Server** row (remembered, applied at the next Play):

- **Just this PC**: the launcher runs a server that only this PC can use.
- **Host for others**: the launcher runs the server for the whole local network and shows what the
  other players enter under Join (**Copy** copies it): this PC's address (choose one if the PC has several)
  and, after a `#`, the start of the server CA's fingerprint, e.g. `192.168.1.20#3f9a0c...`.
- **Join**: the address of a PC that hosts, as its Powder shows it. Nothing runs here but the game. With the
  `#` part, the launcher checks that it reached that host's server (and not someone in between); without
  it, the address alone works too (see Certificate below).

Without a choice, `server_mode` and `server_address` in `launcher.toml` decide.

Every Windows prompt is announced first: the launcher says why it is needed (the hosts file is a protected
system file and firewall rules are system settings, so both need admin rights; Windows asks to confirm a
new certificate) and only continues when you press the button for it.

<details>
<summary><b>What Play does, step by step</b></summary>

Each step only checks unless something is missing.

1. **Game files**: finds the chosen Steep, Ubisoft Connect (and Steam for the Steam version), and checks
   that `Steep.exe` is a build the TLS helper supports.
2. **Firewall** (hosting or joining): Windows Firewall must let the other PCs in. The host needs a rule
   for Powder's server program, every player one for Steep itself (the games connect to each other
   directly). Rules are per program, since Windows asks about any program that listens without one. They
   allow only the local network, only on networks Windows treats as private (Domain or Private, never
   Public: the launcher says how to switch a home network to Private), the server's only on its ports
   (TCP: HTTPS, CRL; UDP: game server, Storm), Steep's only UDP. If one is missing, the launcher explains
   it and adds it after you press the button (Windows asks for admin rights), before the server starts
   listening. It also warns if a rule from an earlier Windows prompt blocks Steep or Powder.
3. **Server**: reaches it, or starts it locally (Just this PC / Host) after checking that its ports are
   free (if not, it names the program using them), with or without the game-data pack as the checkbox says
   (a server it started with another setting is restarted). Joining, it uses the server on the other PC
   (its pack is that server's business), and warns if that server's major version differs.
4. **DNS**: every Ubisoft hostname (`powder/server/hostnames.py`) must resolve to the server address. If not,
   it shows the lines to add to your own DNS server, or adds them to the hosts file for you (the previous
   hosts file is backed up next to it, as `hosts.powder-<time>.bak` in `C:\Windows\System32\drivers\etc`,
   the last 10 kept; lines that are not Powder's are kept byte for byte). A server address given as a
   host name is resolved first: the entries need its IPv4 address.
5. **Certificate**: gets the server's CA, checks that it can only vouch for Ubisoft's names (exactly one
   self-signed CA certificate, path length 0, critical name constraints permitting only names within
   `ubi.com` / `ubisoft.com` and addresses within `127.0.0.0/8`; anything else is refused before Windows
   sees it) and, if Windows doesn't trust it yet, installs it for your user. Each server's CA is pinned
   by its SHA-256 fingerprint (`server_cas` in `data/launcher/settings.json`): the first time, the
   launcher shows the fingerprint and asks (no question for this PC's own server, or when the Join
   address carries a matching `#` fingerprint); if the server later presents another CA, Play stops with
   a warning, since that is either a new CA its host made or someone intercepting the connection to
   read your password. The account calls and client mods use only the pinned CA.
6. **Account**: registers your account on the server, or signs in to one you already have there (the
   launcher remembers the email, never the password).
7. **Client mods**: lists the client mods the server offers and downloads the ones you approved (see
   [Client mods](#client-mods) below). Off unless you turn it on; it never stops Play.
8. **Ubisoft Connect**: restarts it if it is not signed in to the server as this PC's player, then waits
   for its login: sign in with the account's email and password.
9. **Steep**: starts it through Steam or Ubisoft Connect, attaches the TLS helper to the game (then the
   approved client mods) and waits until it is online. Both versions start a short-lived starter process
   first; the helper attaches only to the real game from the chosen install. While the launcher stays
   open, the server keeps running, so Ubisoft Connect stays signed in and the next Play goes straight to
   the game.

</details>

The game-data pack is set up apart from Play, in the **Game data** row under the buttons: **Choose
pack…** installs a `.zip` into the local server's `gamedata/`. Then a checkbox switches it on and off
(remembered in `data/launcher/settings.json`), **Replace…** installs another one and **Remove** deletes
it; changes apply at the next Play.

<details>
<summary><b>The TLS helper</b></summary>

The TLS helper (`native/`) makes the game accept the Powder server's certificate, and only that one:
`hook_guard.dll` traps the game's certificate check (OpenSSL's `ssl_verify_cert_chain`) with a guarded page,
changing no code bytes, and `armer.exe` keeps the trap armed. On each trapped check the DLL accepts the chain
only when the server's certificate is byte-for-byte the one the launcher pinned for this server (it writes
`hook_guard.pin`, the SHA-256 of that certificate's signed body, before starting the game); any other
certificate, or any certificate for another host, is rejected as it would be without the helper. It fails
closed: with no pinned certificate the game stays offline, never trusting more than it should. It only fits
the `Steep.exe` builds listed in `powder/launcher/game.py` (the Steam and the Ubisoft Connect download of the
final update: the same compiled game, packaged differently). Game files are never modified on disk.

</details>

### Client mods

A server can offer client mods: DLLs loaded into your game next to the TLS helper (how a host adds them:
[Modding](#modding)). They are code that runs on your PC with your rights, so the launcher uses none unless you
say so, in `launcher.toml`:

```toml
[client_mods]
enabled = true                 # default false: nothing is downloaded or loaded
allow = ["fov", "hud"]         # exact mod names; no wildcard
```

An allowed mod is used only once you approved it for that server, pinned to its files' hashes. Approving a mod
means trusting that server's host with your PC, as much as running a program they sent you. The pin only makes
sure they can't swap it later without you noticing.

<details>
<summary><b>Approval, pinning and download rules</b></summary>

- With client mods off, the **Client mods** step still shows what the server offers (and the log lists
  it), but downloads nothing.
- An allowed mod is used only once you approved it for that server: the first time, Play stops at the step
  and shows the server, the mod's name, version and description, its files with size and SHA-256 and the
  DLLs it loads, in order. **Play without it** (the first button) plays without it and asks again next
  time; **Approve and load** pins exactly those files and that load order, remembered in
  `data/launcher/settings.json` (`client_mod_approvals`) under the server's pinned CA fingerprint (see
  Certificate above), not its address: another server, or another CA at the same address, asks again even
  for a mod of the same name. Only the version text may change later. (Approvals of older launchers,
  `client_mod_pins`, are not used: each mod is asked about once more.)
- If the host changes an approved mod, Play stops at the step and says what changed: **Approve new
  version** pins the new files, **Play without it** plays this time without that mod (and asks again next
  time). To withdraw an approval, take the mod out of `allow` (its files are deleted at the next Play); to
  forget one, delete its entry under `client_mod_approvals`.
- Files go only to `data/launcher/client-mods/<server>_<port>/<mod>/`. Paths that are absolute, contain
  `..`, a drive letter, a backslash, a colon or a Windows device name are refused, as is any file whose
  size or SHA-256 differs from the server's list (checked after the download and again right before
  loading). Files no longer listed are deleted; game files are never touched. The list is bounded (64
  mods, 256 files and 512 MiB per mod, 256 MiB per file), its texts are cleaned of control characters and
  cut before they are shown, and downloads follow no redirects.
- The DLLs are loaded after the TLS helper is attached and its armer started, mod by mod in the server's
  order. A mod that fails to load is logged and skipped; the game goes on, and the Steep step says which
  mods loaded.

</details>

### Restoring your PC

**Restore system…** undoes everything the launcher set up: it stops the server it started, quits
Ubisoft Connect (which otherwise stays connected to the server), removes the hosts-file entries (backup
kept), the servers' certificates (every one named like a Powder CA, and each one the launcher recorded
installing, `trusted_cas` in `data/launcher/settings.json`) and the launcher's firewall rules (group
"Powder"). Each step asks before a Windows prompt, offers to skip, and only acts if something is there, so it
is safe to run any time. **Play** sets everything up again. `powder --status` and `powder --restore` do the
same from a console. With `remove_hosts_on_exit` / `remove_ca_on_exit` in `launcher.toml`, closing the
launcher offers to take out the hosts entries / the certificate it added in that session.

Only the launcher's own changes are undone. Your data (`data/`, the game-data pack) is kept, and game
files are never changed.

## Ubisoft Connect

Steep only goes online when Ubisoft Connect is online, and without Ubisoft's servers Connect starts in
offline mode. So the server also stands in for what Connect needs.

<details>
<summary><b>Login, demux, cloud saves, certificates, the server's own endpoints</b></summary>

- **Login**: Connect's `POST /v3/profiles/sessions` (email and password of a Powder account, then the
  remember-me ticket it got; passwords are never logged) and its other UbiServices calls. The game's own
  login follows the player Connect signed in as on the same PC.
- **Demux** (`dmx.upc.ubisoft.com`): Connect's own protocol for login, owned games, friends, play
  sessions, achievements and ownership tokens (`transport/demux/`). It shares port 443 with HTTPS: the
  listener routes by the requested hostname (`transport/tls_router.py`).
- **Internet check** and CORS for Connect's web UI.
- **Cloud saves** (`connect_cloud_saves`): Connect uploads the game's own save files at game exit and gets
  them back at the start on another PC (`services/cloudsaves.py`, files in `data/cloudsaves/`). A cloud save
  is only offered while all its files are there; `powder-server admin cloud-saves [PLAYER] --delete`
  removes one.
- **Certificates**: Connect checks them through Windows, so the server generates a local CA in
  `data/tls/` ("Powder (Steep) local CA" in Windows' certificate list), restricted by name constraints to
  `ubi.com` / `ubisoft.com` / loopback, so it cannot vouch for any other website. The launcher installs it;
  by hand:

  ```
  uv run powder-server admin trust install    # current user's trusted roots; Windows asks to confirm
  uv run powder-server admin trust status
  uv run powder-server admin trust remove
  ```

What is not emulated (friends' invites, Connect's news and store pages): see [Game features](#game-features).

The server's own endpoints: `GET /_powder/status` (connected Connect clients and game sessions, and the
caller's address; other players' addresses only for the server's own PC, and on a public server only the
caller's own connections), `GET /_powder/ca.pem` (the CA certificate) and `/_powder/account/*` (the
launcher registers and changes accounts there; every call carries the account's password). Web pages from
other sites (the hosts file sends the browser here too) may only GET; CORS is for Ubisoft Connect's origins.

</details>

A server reachable from the internet should set `[server] public = true`.

## Rust server

The server is ported to Rust (`server/`), reply for reply: the game and Ubisoft Connect must not notice the
difference. It is complete and the **default** for powder-wrap and the Linux launcher; the Python server stays
available (`POWDER_SERVER=python`) and is the reference the port is tested against. The Windows launcher still
runs the Python server.

```
cargo build --release                              # target/release/powder-server
powder-server run                                  # same config.toml and data/ as the Python server
powder-server mods                                 # load the mods and show what they hook
powder-server admin trust status                   # also install / remove
powder-server admin accounts                       # accounts, account-create, account-password, ...
powder-server gamedata verify                      # also build / zip / install
```

Everything of the Python server is ported: HTTPS (all routes, the websocket), Ubisoft Connect's demux, NEX
(login, stats, matchmaking, channels), Storm, accounts, the importer, the admin CLI. Only the Rust server has:

- **mods**: Lua server mods and serving client mods ([Modding](#modding)), including the
  [admin page](#admin-page);
- **[bans, the whitelist and kicks](#bans-whitelist-and-kicks)**;
- Steep's "intro finished" item, which it grants (the game kept replaying the Alps intro without it).

Both mark the DLC worlds as owned. The Rust server reads the same `config.toml`, `data/` (database, keys, local
CA) and game-data pack as the Python server, so you can switch between them. `admin trust` also works on Linux
(see [Linux](#linux-lutris-proton) for Wine prefixes).

## In-game overlay and trainer

`native/overlay/` (built by `native/build.sh`, loaded with `POWDER_OVERLAY=1`) is a Dear ImGui overlay inside
Steep, toggled with **Home**:

- **Server**: the admin actions of `/_powder/admin` (players, credits/items, stats, broadcasts) - Rust server.
- **Debug**: connection status, the broadcast feed, FPS, an HTTP log, a memory scanner.
- **Trainer**: finds the rider's position by itself (pointer chains), **Fly** (F6; W/A/S/D, Shift up, Ctrl
  down) with **noclip** (the game's own writes to the position are hooked out, so physics and collision can't
  move you), teleport spots, freezable address slots.
- **Mods**: the client mods it hosts, their hooks and panels.

Fly moves where the camera looks. The camera is found live: turn it once while standing still. Camera-relative
Fly is confirmed in-game; its calibration has since been made more robust (it waits for the rider to stand still
and keeps every agreeing copy of the camera), which is not yet confirmed in-game.

The overlay also logs crashes with the faulting module+offset to `powder_overlay.log`, and carries small game
fixes (a guard for a crash in the DLC worlds). The reverse-engineering tools behind it are in `tools/trainer/`
(external memory scans, perf watchpoints with call stacks, a dump of the unpacked game for disassembly).

## Modding

A mod is a folder `mods/<name>/` with up to three parts; the full guide is
[native/sdk/README.md](native/sdk/README.md). `mods/` is next to `config.toml`; `[mods]` in the config picks
which are used, and names starting with `_` are skipped (see `mods/_example/`). Server mods need the Rust server.

| Part | What | Status |
| ---- | ---- | ------ |
| `mod.lua` | **server mod** (Lua): events, HTTP/NEX hooks, admin tools | ✅ (e.g. `mods/admin`) |
| `client/*.dll` | **client mod** (native, hosted by the overlay): function and mid-function hooks on the game's code, per-frame logic, overlay panels, talking to its server half (`native/sdk/powder_api.h`) | ✅ (e.g. `mods/asset_trace`) |
| `client/forge/<Base>/` | **asset mod**: entries of Steep's `.forge` archives that override the game's, loaded from a per-session `<Base>_patch_03.forge` | ✅ textures, meshes, material swaps and new geometry from Blender |

Players choose which client mods reach their game: on Linux, powder-wrap loads the ones named in
`POWDER_CLIENT_MODS` from this repository (DLLs through the overlay, assets as a `_patch_03`); on Windows the
launcher loads only mods the player approved, pinned to their hashes ([Client mods](#client-mods)). **A server
cannot push code to its players.**

**Example mods**

| Mod | What | Seen in-game |
| --- | --- | --- |
| `mods/_example` | a server mod skeleton | - |
| `mods/admin` | the [admin page](#admin-page) | - |
| `mods/asset_trace` | a client mod: hooks ntdll's file calls, a panel, a log | ✅ |
| `mods/custom_texture` | a new texture on the menu rider's jacket | ✅ |
| `mods/mesh_edit` | the jacket's mesh changed (same topology, all 4 LODs) | ✅ |
| `mods/model_swap` | a material swap | ✅ |
| `mods/shoulder_pads` | new geometry modelled in Blender, with its own weights: the pads follow the arms | ✅ |
| `mods/master_chief` | a costume from a downloaded model (CC-BY), fitted onto Steep's skeleton; replaces costume 016 | not yet in this version |

Asset mods built from the game's own data are generated from your install by each mod's `generate.sh`;
nothing of Ubisoft's is committed.

**Still open:** new items with their own ids (today a model replaces an existing item), and decoding the
skeleton resource (the bone hierarchy).

<details>
<summary><b>Server mods (Lua)</b></summary>

`mods/<name>.lua` or `mods/<name>/mod.lua`, returning `function(api) ... end` (Lua 5.4; they replace the
Python server's plugins).

- `api.on(event, fn)` for `login`, `nex_connected`, `purchase`, ...
- `api.before_http(method, path_glob, fn)` and `api.before_rmc(method, fn)` answer requests themselves (return
  nil to let the server answer).
- `api.save` / `api.load` keep data in the server's database; `api.log`, `api.config`.
- Errors in a mod are logged and never reach the game.
- Server mods run with the host's trust; as a safety net against bugs they get a reduced Lua library (`string`,
  `table`, `math`, `utf8`, `coroutine`, `os.time`/`clock`/`date`; no `io`, `os.execute`, `require`, `dofile`,
  `debug`), share at most 128 MB, and a hook that runs longer than 100 ms is aborted (logged).

```lua
-- mods/welcome.lua
return function(api)
    api.on("login", function(s) api.log.info(s.name .. " signed in") end)
end
```

`powder-server mods` loads the mods and shows what they hook.

</details>

<details>
<summary><b>Client mods (native DLLs)</b></summary>

Files in `mods/<name>/client/` and a `mod.toml` (`version`, `description`, `[client] inject = ["x.dll"]`).
The server lists them at `/_powder/mods`; the launcher downloads them and loads the DLLs into the game.
Sources in `mods/<name>/src/*.cpp` are built by `native/build.sh mods` into `mods/<name>/client/<name>.dll`.
The API (`native/sdk/powder_api.h`): hooks (function detours and mid-function hooks with registers), code
patches, pattern search, safe memory reads and writes, the player's position, per-frame callbacks, overlay
panels (Dear ImGui), and HTTP calls to the mod's server half under `/_powder/`. Details:
[native/sdk/README.md](native/sdk/README.md).

</details>

<details>
<summary><b>Asset mods and the tools (Blender)</b></summary>

The asset tools: `tools/forge/forge.py` (the archives: list, extract, swap, build, and `where` to find the
datafiles holding a resource), `texture.py` (textures to and from PNG, all their formats), `mesh.py` (meshes to
and from OBJ and glTF, also with a new topology), `tools/savegame/` (Steep's save files).

**Blender**: `mesh.py export ... -o item.glb` gives the item with its armature, weights and texture; model,
weight-paint or attach parts to bones in Blender, and `mesh.py import --glb` turns the result back into the
game's mesh. The Blender add-on `tools/forge/powder_blender.py` does the same from a sidebar tab in Blender
(Import item / Write mod). Guide: [tools/forge/BLENDER.md](tools/forge/BLENDER.md).

powder-wrap builds `<Base>_patch_03.forge` from all named mods' entries for the session and removes it
afterwards.

</details>

## Files

Installed, Powder's folder is `%LOCALAPPDATA%\Powder`; from source, the current folder.

<details>
<summary><b>What is in it</b></summary>

- `config.toml` and `launcher.toml`: optional settings (see `config.example.toml`, `launcher.example.toml`)
- `gamedata/`: the game-data pack
- `plugins/`: Python server plugins; `mods/`: mods (Rust server)
- `data/`: everything the server generates on first run:
  - `powder.db` (players, accounts, bans and the whitelist)
  - `logs/server.log`
  - `bodies/` (logs the game uploads)
  - `tls/` (local CA and server certificate)
  - `nex_signing_key.pem`
  - `cloudsaves/` (cloud saves)
  - `launcher/` (launcher log, hosts-file backups, the server's CA as downloaded by the launcher, approved
    client mods in `client-mods/`)

</details>

## Ports

| Port | Protocol | What                                                                                           |
| ---- | -------- | ---------------------------------------------------------------------------------------------- |
| 443  | TCP      | HTTPS for the game and Ubisoft Connect, and Connect's demux (one listener, routed by hostname) |
| 6005 | UDP      | Quazal NEX (the game's online session)                                                         |
| 80   | TCP      | the local CA's revocation list (`http://public-ubiservices.ubi.com/powder-ca.crl`)             |
| 11000-11003, 11005 | UDP | Storm, the games' peer-to-peer helpers: NAT detection (11000-11003) and traversal (11005) |

All of them are set in `config.toml` (`storm_port` is the first of the Storm ports). By default the server only
listens on this PC (`bind = "127.0.0.1"`). To host for the local network, choose **Host for others** in the
launcher: it starts the server with `--bind 0.0.0.0 --public-host <this PC's address>` and adds the firewall
rule. Without the launcher, set `bind = "0.0.0.0"` and `public_host` in `config.toml` and allow the server in
Windows Firewall yourself.

<details>
<summary><b>Peer-to-peer and Storm</b></summary>

The players' games also talk to each other directly (UDP, peer-to-peer, ports the game picks). On a
local network that needs nothing but the firewall rule for Steep. The game only starts its multiplayer
part when its configuration names peer-to-peer helper servers (Storm), and it finds the other players'
games through them: NAT detection tells a game the address the server sees, and the traversal server
passes a game's "connect me to that player" on to the other game, so both send to each other at once
(hole punching). Powder runs both itself (`powder/server/transport/storm.py`, as recorded from
Ubisoft's) and tells the game they are at the server's address (`[multiplayer] storm = "powder"`, the
default; `"off"` switches the game's multiplayer off). No game-data pack is needed for it.

</details>

## Progression

The game computes its progress itself and uploads it as stat events
(`PlayerStats2Protocol::PostEvents_V1`); `services/stats.py` maps each event to the stat it updates
(`EVENT_RULES`). The rules are monotonic (max / add / bitwise-or), so a stale upload can never lower a
level or wipe an unlock. Every upload is also kept raw in `stat_events`. When your own recordings are
present, `tests/test_recordings.py` replays them to check the rules reproduce every recorded change.

## Internals

<details>
<summary><b>How a request is answered</b></summary>

|     | HTTPS                                                 | NEX                                         |
| --- | ----------------------------------------------------- | ------------------------------------------- |
| 1   | plugin `before_http` hook                             | plugin `before_rmc` hook                    |
| 2   | route with real logic (`transport/https/routes_*.py`) | handler (`transport/nex/handlers/*`)        |
| 3   | game-data pack reply (configuration)                  | game-data pack reply (`rmc.json`)           |
| 4   | `{}` + `UNMATCHED` in the log                         | RMC error `Core/2` + `UNHANDLED` in the log |

In the Rust server, step 1 is the mods' `api.before_http` / `api.before_rmc`.

</details>

<details>
<summary><b>Extending the Python server</b></summary>

- **New NEX method**: add a function to a module in `transport/nex/handlers/` (or a new module listed
  in `handlers/__init__.py`): `fn(ctx, params: Reader) -> bytes` (may be `async`). `ctx.profile_id`,
  `ctx.services`, `ctx.conn`. Raise `Fallthrough` to use the pack's reply instead.
- **New HTTPS endpoint**: add a route in `transport/https/routes_*.py`; keep logic in `services/`.
- **Schema change**: add `db/migrations/000N_name.sql`; it is applied on the next start.
- **Plugin**: drop `plugins/name.py` with `setup(api)`; see `powder/server/plugins/api.py`.
- **Matchmaking** policy (who meets whom) is in `services/matchmaking.py`; the game's calls and the
  notifications it expects are described there.

</details>

## Licence

Powder is free software under the [GNU Affero General Public License v3.0 or later](LICENSE): you may
use, change and share it; if you run a changed version as a server for others, you must offer them its
source. It comes without any warranty (see [Disclaimer](#disclaimer)). The licence covers Powder's code only;
Steep, its data and the game-data pack belong to Ubisoft. The installer includes the licences of the bundled
libraries (`THIRD_PARTY_LICENSES.txt`).
