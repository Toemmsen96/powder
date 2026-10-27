# Content mods that combine (design, 2026-10-27)

Status: phases 1-2 (several costumes per run, own names and pictures) CONFIRMED in-game; phase 3 `tools/forge/compose.py`
built and tested outside the game (two costume mods + a world-edit mod in one build, cached, refusals); the
content.toml keys it reads are in its docstring. Grants: `<data>/content_grants.json` (Rust server).
Phase 4 CONFIRMED in-game 2026-10-27: powder-wrap runs compose before the server for the named content mods
(mods/costume_pack_test from the Linux launcher): both costumes listed with their names and pictures, Chief owned
through the server's grant. Old-style asset mods that add items get no list entries any more (compose writes the
list from the content mods): use content mods for new items.

Goal: mods that **add** things (costumes, gear, world objects, texts) are drop-in folders, and any set of them works
together, like ReSkate's mod folders for Skate 3. Today an asset mod ships whole copies of the entries it changes
(`client/forge/<Base>/`): two mods that each add a costume both ship their own copy of Game Bootstrap Settings and the
character templates, the newest copy wins, and only one costume survives. The custom item list
(`<data>/item_list.json`) is one hand-made file. Combining needs a merge step, so mods declare additions instead of
shipping patched shared entries.

## A content mod

```
mods/<name>/
  mod.toml          name, version, description, author, license (as today)
  content.toml      what the mod adds (below)
  assets/           its own files: models (.glb), pictures (.png), world edit lists (.json)
  client/forge/     optional: plain replacements of entries no other mod should touch (as today)
```

```toml
[[costume]]
name  = "COSTUME_MF_POWDER_MASTER_CHIEF"     # the new item's name: its ids, GUID and loc id derive from it
like  = "COSTUME_MF_URBAN_OUTFIT_JAPAN_139"  # the costume it is cloned from (model slots, category, selectors)
store = "COSTUME_M_MILITARY_GREY_94"         # the item-list entry it copies (type, tags: where it is listed)
title = "Master Chief"                        # its name in every language
model = "assets/chief.glb"                    # optional: a fitted model for the clone's meshes
icon  = "assets/icon.png"                     # optional: its picture in the costume list
price = 0
grant = true                                  # every player owns it (else: a free offer in the store)

[[world]]
map   = "Alpes"
edits = "assets/alps_park.json"               # the world editor's edit format (world.py apply)
```

## Who does what

The build needs the player's own game install (Steep's data is never shipped), so it runs in the **launcher** on the
player's machine: the Windows launcher (powder/launcher) and powder-wrap on Linux call the same Python module
(`tools/forge/compose.py`). The split:

| part | job |
|---|---|
| server | which content mods it runs; hands their declarations and assets to launchers (`/_powder/mods`); serves the item list, store offers and grants built from them - the same for every player, so riders see each other's custom gear |
| launcher | fetches the server's content mods, builds them against the local install (cached), packs the session's `_patch_03` archives |

Content mods are **data only** (`content.toml`, models, pictures, edit lists): Powder's own tools turn them into game
files, so a launcher can take them from a server without running anyone's code (the parsers must treat them as
untrusted input: size limits, no paths outside the mod). Mods with code (client DLLs, a model-fitting script like
master_chief's make_chief.py) keep today's per-player approval; for a content mod such a script runs on the
author's machine and the mod ships its result (e.g. the fitted .glb).

## The launcher's mod screen

The launcher shows the server's mods before it connects (the server's `/_powder/mods` list, extended):

| per mod | shown |
|---|---|
| name, description, author, license, size | from `mod.toml` |
| what it adds | from `content.toml` ("2 costumes", "world edits in the Alps"), or "code: client DLL" |
| required / optional | required: content in the server's item list or world (every player needs it, else items are missing for them and riders don't see each other's gear); optional: client-side extras (world editor, tools) |
| status and next step | download (size), building against your install, ready, update available, needs approval (code) |

Required mods must be ready before the launcher connects. Data-only mods download with one click (or automatically,
if the player allows that for the server); mods with code always need an explicit approval and stay pinned to the
approved hashes (today's rule). Linux: powder-wrap runs without a window under Lutris, so it shows the same list in a
small dialog before the session (zenity/kdialog), and the overlay's Mods tab lists what the session loaded.

## The session build (`tools/forge/compose.py`, run by the launcher before the asset mods are packed)

1. Read the named mods' `content.toml` in `POWDER_CLIENT_MODS` order; refuse duplicate item names; report two mods
   replacing the same entry (the later one wins, as today, but it is named in the log).
2. **One** pass over each shared resource for all mods together: Game Bootstrap Settings (all new items, gear lists,
   tag registry), the four character templates (all Clothing branches and selectors), the localization packages
   (all titles), the icon textures; per-item entries (entities, materials, textures) come from each mod.
3. The item list: the game's list (captured once by `storemap.py --item-list` into `<data>/game_item_list.json`) plus
   every custom item -> `<data>/item_list.json`; store offers and grants for the server.
4. World edits: every mod's lists plus the world editor's -> one `world.py apply` per map.
5. Output: one folder per `<Base>` for powder-wrap to pack into `<Base>_patch_03.forge`; cached by a hash of the
   inputs (the game install's archives + the mods' content), so an unchanged set starts at once.

Ids: every new id derives from the item's name (as `items.py add-costume` does), so two mods never collide unless
they use the same name, which step 1 refuses. The server serves whatever list the build wrote; the list's version
changes with the set of mods, so the game downloads it again when mods are added or removed.

Order of work: `items.py add-costume` for several items in one run (shared resources patched once); loc.py for
titles; compose.py; powder-wrap's step; mods/custom_costume_test rewritten as the first content mod.
