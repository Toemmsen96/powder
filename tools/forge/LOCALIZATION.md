# Steep's text: LocalizationPackage_<Language>

`loc.py` reads and writes the game's text. It uses `forge.py` from the same folder; archives are only read, and
`_patch_NN` past `_patch_02` (asset mods' session archives) are left out.

```
loc.py list   <Steep> [-l English] [--subtitles] [--json]   # every string: id, text
loc.py find   <Steep> TEXT [-l all] [-i]                     # e.g. find "japan outfit" -i
loc.py get    <Steep> 0x4ed2 [-l all]                        # one string, in every package with -l all
loc.py set    <Steep> ID TEXT [ID TEXT ...] -o mods/<mod>/client/forge/DataPC [-l French]...
loc.py new-id <Steep> NAME                                   # the id items.py add-costume --title gives NAME
loc.py test   <Steep>                                        # decode + rebuild every package copy, compare bytes
```

`set` writes every UI package that holds text (or those named with `-l`), with the strings added or changed, as
packed `.datafile`s that powder-wrap builds into `DataPC_patch_03.forge`. Two mods that both write packages
collide (the archive takes one entry per id), like two mods that both patch Game Bootstrap Settings.

**C** = confirmed on the shipped data (every package, unless a sample is named), **G** = a guess.

## The packages (C)

- Resource type 0x6E3C9C6F, one per datafile entry, in the DataPC stack (base, `_patch_01`, `_patch_02`; the newest
  copy is the game's). 17 UI packages hold text (English, French, German, Italian, Spanish(Spain), Polish, Russian,
  Dutch, Brazil, Japanese, Korean, Chinese(PRC), Simplified_Chinese; Danish, Norwegian, Swedish and Finnish hold only
  233 strings each); each has a `_Subtitles` twin (Spanish(Spain)'s is `Spanish_Subtitles`); 13 packages are empty
  (41 bytes: Arabic, Czech, Hungarian, Portuguese, Spanish(South America), LocTest, their `_Subtitles`, and
  English(US)_EManual).
- **String ids are global.** An item's LocString (ITEMS.md) holds one: 0x4ed2 = "GREEN MOSAICS - JAPAN OUTFIT"
  (shown in the game as "Green Mosaics - Japan Outfit": item names are stored in capitals, G: the menu sets the
  case). UI ids run 6,637..28,500, subtitle ids 5,683..28,014; the two sets are disjoint. The full UI packages all
  span the same ids (not every language has every string).
- `LocalizationManager_0XA8BEFF45` (type 0x4A70DE43, Game Bootstrap Settings) lists 6,675 records `u64 0, class
  0x69A90F1E, u32 string id, u32 0`, all of them subtitle ids, and then links the packages (UI and subtitles). It
  names no UI string, so a new UI string needs no entry there.

## Payload

Little-endian up to the text object, big-endian inside it.

| offset | field | |
|---|---|---|
| 0x00 | u64 id, u32 type 0x6E3C9C6F, u32 1 | C |
| 0x10 | u8 0, u8 **language** (1 English, 2 French, 3 Spanish, 4 Polish, 5 German, 6 Chinese(PRC), 7 Simplified Chinese, 9 Italian, 10 Japanese, 12 Korean, 13 Russian, 14 Dutch, 15 Danish, 16 Norwegian, 17 Swedish, 19 Brazil, 20 Finnish), 14 zero bytes | C values, G meaning |
| 0x20 | u8 0, u32 class 0xD28389B5 (the text object), u32 its size (0: an empty package) | C |
| 0x29 | the text object: | |
| +0 | u8 0, u8 **b1** (codes below b1 take one byte), u16 **n** (dictionary entries) | C |
| +4 | n x (u16 x, u16 y): y = 0: a leaf, the UTF-16 code unit x; else a pair: text(y) followed by text(x). Entry 0 is the NUL leaf | C |
| | u16 g (banks), g x (u32 **first id**, u32 data offset, u32 table offset; offsets from +0) | C |
| | per bank its table: u16 m, m x (u16 offset in the bank's data, u16 id - first id), u16 data length. The bank's first string (offset 0, id = first id) has no record | C |
| | the banks' data, one after the other: each string's codes, no terminator (a string ends where the next starts) | C |

- **Codes**: a code v names entry v + 1 (entry 0 is never coded). v < b1: one byte v; v < b1 + (255 - b1) * 256: two
  bytes `b1 + (v - b1) / 256, (v - b1) % 256`; else three bytes `ff, v >> 8, v & ff`. b1 is 0xFE in most packages,
  0xF9..0xFD in the Chinese, Japanese and Korean ones (more two-byte prefixes), 0xE6 in Norwegian.
- **Dictionary**: 248..2,655 entries; the pairs spell syllables and words (" the ", "ing", " SNOWBOARD"), the leaves
  are the package's alphabet. A pair's children may come later in the table.
- **Banks**: strings sorted by id, filled greedily: a bank takes the next string while its data stays at most 65,535
  bytes (offsets are u16) and the id is at most 65,535 above the bank's first id. English has 3 banks (first ids
  6,637 / 13,006 / 23,065). The rebuild reproduces every shipped split.

`loc.py test`: all **98 package copies** of the stack (46 in DataPC, 35 in `_patch_01`, 17 in `_patch_02`; 448,706
strings) decode and **rebuild byte for byte**. The encoder (the shortest code sequence over the dictionary's texts)
re-codes every string of the 34 newest packages with text to the same text, never in more bytes than the shipped
codes (fewer for 1% to almost half of a package's strings: Ubisoft's coder does not always find the shortest).

## New text

- New strings use the package's own dictionary; a code unit the package lacks gets a new leaf at the end of the
  table (shipped entries keep their numbers, so every shipped string's codes stay valid). "Master Chief" needs no new
  entry in any of the 17 UI packages.
- **New ids** (`new_string_id`): one of the unused ids inside the UI range (6,637..28,500; 3,405 free), picked by
  crc32(NAME) so the same name gets the same id each time; `items.py add-costume --title` keys it by the item's name
  (COSTUME_MF_POWDER_MASTER_CHIEF: **0x1f43**). TRIED in-game 2026-10-27: an id past every shipped one (0x86ef,
  appended to the last bank) crashed the game at start; renaming a shipped id (0x4ed2 -> "MASTER CHIEF") works.
  CONFIRMED in-game 2026-10-27: a new id inside the range (0x45f2 "POWDER OVERALLS" on a cloned costume, next to the
  renamed 0x4ed2) shows its name, and the original keeps its own. (The phase-1 crash blamed on in-range ids came
  from a source with an unlock condition, see ITEMS.md.)