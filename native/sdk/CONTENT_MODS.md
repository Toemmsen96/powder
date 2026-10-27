# Content mods that combine (design, 2026-10-27)

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

## The session build (`tools/forge/compose.py`, run by powder-wrap before the asset mods are packed)

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
