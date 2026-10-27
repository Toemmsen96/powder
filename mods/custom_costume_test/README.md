# custom_costume_test: a new costume, nothing replaced

This mod tests whether Steep takes **new** content: a costume item the game never had, with ids of its own, next to
the game's costumes. Every mod so far replaced an existing entry; this one only adds. It is a clone of the "urban
outfit Japan" costume (costume 016, item `COSTUME_MF_URBAN_OUTFIT_JAPAN_139`), so it looks like that outfit, but
every part of it is new:

| new | id | where |
|---|---|---|
| item `COSTUME_MF_POWDER_TEST_01` (price 0, the starter-gear flag set), listed in GEAR_COSTUMES | `0x1f07518a000` | Game Bootstrap Settings (DataPC) |
| model tag (registered in the tag registry) | `0xdc48f11c` | the item, the Clothing branches |
| a branch in STP_M_Clothing and STP_F_Clothing, selectors `STP_M/F_Costume_PowderTest01`, `STP_MF_Costume_PowderTest01_Textures` | `0x1f07518a00a..0c` | the 4 character templates (DataPC, DataPC_extra_chr) |
| entities `STP_M/F_Costume_PowderTest01` with 4 LODs each, 3 materials, 12 textures, 24 Mips | `0x1f07518a0..` | DataPC_extra_chr |

The ids come from the item's name, so they stay the same each time the mod is generated. `client/forge/
COSTUME_MF_POWDER_TEST_01.json` lists all of them (old -> new). How items, selectors and ids work:
tools/forge/ITEMS.md.

## Generate

```
mods/custom_costume_test/generate.sh [<Steep folder>]            # the clone of costume 016 (about 20 s)
mods/custom_costume_test/generate.sh --chief [<Steep folder>]    # Master Chief as a new item (see below)
```

It writes about 220 MB into `client/forge/` (gitignored): `DataPC/` (the bootstrap and the two NPC templates, packed)
and `DataPC_extra_chr/` (the two templates, packed, and 41 cloned entries). Then `items.py check` re-reads it against
the game (0 problems expected). powder-wrap builds `DataPC_patch_03.forge` and `DataPC_extra_chr_patch_03.forge` from
it for the session; enable it with `POWDER_CLIENT_MODS=custom_costume_test` (restart Lutris after changing it).

`--chief` clones the costume that `mods/master_chief` converts, from that mod's generated entries
(`mods/master_chief/generate.sh` first). The new item is called `POWDER_MASTER_CHIEF`. If master_chief converts a
bundle costume (costume 005, the penguin), the new item is a **jacket** (the penguin bundle draws its body through
its jacket item `JACKET_MF_C_Penguin_228`): look for it under jackets. Leave the master_chief mod itself off to see
Chief only on the new item.

## Master Chief as a new, owned costume (2026-10-27)

The plain clone never showed up: the menu and store list only items with a store GUID, and the GUIDs come from
Ubisoft's item list (ITEMS.md), whose entries name their game object by a tag. The list has entries no game object
uses; `--chief` now names the new item after one of them, `PROGRESSION_TOKEN_BEAVER_COSTUME_ULC_CONDITION_ULC_4026690459`
(GUID `077f575a-e943-4d28-8cc4-7d637a54dce0`), so the game should tie that GUID to it (G). Made with

```
mods/master_chief/make_chief.py --game <Steep> --target urban_japan -o build/chief_016   # a full costume model
CHIEF_SRC=build/chief_016 mods/custom_costume_test/generate.sh --chief
powder-server admin grant <player> 077f575a-e943-4d28-8cc4-7d637a54dce0 1                # own it
```

and a free offer in `data/store_items.json`. In the game, with only this mod on (master_chief off, so the military
costume looks normal): Costumes should list one more costume (the urban outfit's name and icon, Chief's body).

## What to check in the game, in order

1. **The game starts and the menu loads** with the mod on. A crash or hang at start points at
   `DataPC_patch_03.forge` (the patched Game Bootstrap Settings); turn the mod off and tell. (asset_trace cannot show
   whether DataPC_patch_03 is read: the overlay hooks file reads only after the game has opened the DataPC
   archives, mods/model_swap/README.md. Step 2 is the test of that.)
2. **Costumes & Equipment -> Costumes**: is there one more costume than without the mod? It has the urban outfit's
   name and icon (the clone keeps them), so it may show as a second "urban outfit" entry, or as the only one if the
   game does not offer the original (the three urban outfit items have no store GUID). Note whether it shows as
   owned, locked, or with a price.
3. **Equip it**: the rider should wear the urban outfit (male and female riders both). Showing the default clothes
   instead means the item is found but its selector is not; a crash on equipping points at the cloned entity or
   materials.
4. With `--chief`: Master Chief on the new item, and the game's own costume unchanged.

Report what you see at each step, plus `powder_overlay.log` and `$XDG_RUNTIME_DIR/powder-wrap.log` (the session's
archive builds are logged there).

## Assumed (the live test decides)

- The game loads `DataPC_patch_03.forge` like the `_patch_03` of DataPC_extra_chr and the maps (seen), and the newest
  copy of `Game Bootstrap Settings` wins.
- The gear menu lists GEAR_COSTUMES (not only items the store catalogue knows), and an item with price 0 and the
  starter-gear flag counts as owned without a store GUID. Both are guesses (ITEMS.md, "Item").
- New ids above the shipped range and a new tag are accepted; the new selectors are found through the Clothing
  branch and the depth-first lists; the tag registry entry is enough (or not needed).
- The patched templates' unknown name-record hash (0) and the copied GlobalMetaFile are accepted (as for every asset
  mod so far).

## Undo

**Before turning the mod off, put another costume on** (or none): the save then holds no id the game does not know.
What the game does with a save that names a missing item is unknown. Then leave the mod out of
`POWDER_CLIENT_MODS`. The installed game files are never changed; the `_patch_03` archives exist only for the session.
