# forge: Steep's .forge archives

`forge.py` reads Steep's `.forge` archives (Ubisoft AnvilNext, which the engine calls "Scimitar"). It lists
entries, extracts them raw or decompressed, splits datafiles into resources, shows which of a base archive and
its `_patch_NN` siblings provides each entry, and checks checksums. It can also build datafiles and archives
again. This is groundwork for asset mods. The game has never loaded an archive that `forge.py` built.

Run it with the system `python3` (3.10+). zstd needs `zstandard` or `pyzstd`; LZO1X is built in. The tool only
opens archives for reading and seeks inside them, so it never loads a multi-GB archive into memory. Point it at
the game folder (on the first PC that is `~/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft
Game Launcher/games/Steep`). Never build into that folder: write new archives somewhere else.

```
forge.py info    DataPC_extra_patch_02.forge                 # header, tables, entry types
forge.py list    DataPC_extra.forge [-g HELMET] [-t a2b7e917] [--json]
forge.py show    DataPC_extra.forge HELMET_MF_N_094_IconList_MapDesc   # blocks, resources (id, 0x.., name, substring)
forge.py extract DataPC_extra.forge <entry>... [--all] -o out/       # raw .datafile + unpacked folder
forge.py pack    out/<id>_<name>/ -o modded.datafile                 # unpacked folder -> datafile
forge.py stack   DataPC_extra.forge [--entries | -g NAME]            # base + _patch_NN, who provides what
forge.py verify  DataPC_extra.forge [--decompress]
forge.py build   OUT.forge --like DataPC_extra.forge <datafile or unpacked folder>...
forge.py swap    DataPC_extra_chr.forge 0xeac3434be5 --with DataPC_extra_chr.forge 0x9c6dd7015f -o out/  # see below
```

Example output:

```
$ forge.py list DataPC_extra_patch_02.forge
0x0000000000000010  off=0x000020815a  size=      436  0x00000000               2019-06-06  GlobalMetaFile
0x000000fda9f181ad  off=0x00001d9faf  size=    14696  0x989dc6b2 MapDesc       2019-06-06  HELMET_MF_N_094_IconList_MapDesc
0x000000fda9f181ac  off=0x00001dd917  size=    14597  0xa2b7e917 TextureMap    2019-06-06  HELMET_MF_N_094_IconList_Map
$ forge.py show DataPC_extra_patch_02.forge HELMET_MF_N_094_IconList_MapDesc
HELMET_MF_N_094_IconList_MapDesc  id=0xfda9f181ad  index=1  offset=0x1d9faf  size=14696  type=0x989dc6b2 MapDesc ...
  header block: zstd, 1 chunk(s), 30 -> 30 bytes, bad checksums 0
  body block: zstd, 1 chunk(s), 14604 -> 66000 bytes, bad checksums 0
  2 resource(s):
    0x000000fda9f181ad  0x989dc6b2 MapDesc       payload=      254  HELMET_MF_N_094_IconList_MapDesc
    0x000000fda9f181ac  0xa2b7e917 TextureMap    payload=    65660  HELMET_MF_N_094_IconList_Map
$ forge.py stack DataPC.forge -g Bootstrap
(GlobalMetaFile and PrefetchingFileInfos, which every archive has, left out)
DataPC.forge: 1290 entries, 0 override earlier archives, 1290 new, 994 effective
DataPC_patch_01.forge: 391 entries, 296 override earlier archives, 95 new, 341 effective
DataPC_patch_02.forge: 59 entries, 50 override earlier archives, 9 new, 59 effective
0x0000000000000800  Game Bootstrap Settings      <- DataPC_patch_02 > DataPC_patch_01 > DataPC
```

`extract` writes `<id>_<name>.datafile`, which holds the entry's raw bytes and can be fed back to `build`
unchanged. Next to it goes a folder with one `.payload` file per resource and a `datafile.json` (ids, types,
names, the opaque `pre`/`extra` fields). Edit a payload and `pack` the folder, or give the folder to `build`
directly.

## Format

All integers are little-endian. **C** = confirmed: it holds on every shipped archive (27 files, 165,021
entries) or on the samples stated. **G** = a guess, or an observation whose meaning is unknown.
`forge.py verify --decompress` passes on all 27 archives with 0 problems (2026-10-26). That run
decompresses 427,423 chunks, checks every checksum, splits every datafile into resources whose sizes add up,
and checks that each entry's first resource has the entry's id and type.

### File layout (C)

| offset | contents |
|---|---|
| 0x00 | `"scimitar\0"`, u32 version = 27, u64 archive-header offset (always 0x41A), u64 16, u64 1, zeros (G: constants) |
| 0x41A | archive header (44 bytes): u32 entry count, u32 2, u64 0, u32 0, i64 -1, u32 5000 (slots per table), u32 table count, u64 first table offset |
| table | table header (48 bytes): u32 entries in this table, u32 folder count (2 in table 0, else 0), u64 index offset, i64 next table (-1 = last), u32 first entry no., u32 last slot no. (first + 4999), u64 name table offset, u64 folder table offset |
| index | 5000 slots x 20 bytes: u64 data offset, u64 entry id, u32 size. Unused slots are zero. |
| names | 5000 slots x 192 bytes, parallel to the index (see below) |
| folders | 5000 x 176 + 1 bytes. The same in every archive except the root's file count (G: an editor folder tree, the root plus `_Lost&Found`) |
| data | the entries, packed one after another. Small gaps can occur: a base archive also has an unreferenced old GlobalMetaFile copy right after the folders, and a rewritten entry may sit in a slot larger than itself. |

An archive with more than 5000 entries chains its tables (`DataPC_extra_chr` has 4). The extra tables sit in
the middle of the data area, and entry numbering runs on across them.

Name record (192 bytes):

| +off | field |
|---|---|
| 0x00 | u32 size. Equals the index size, except in 5 entries where an entry was rewritten into a bigger slot: `DataPC_extra_patch_02`'s prefetch list says 280, but the index says 169. |
| 0x04 | u64 hash. G: unknown. It changes with every build of an entry and is not xxh64/xxh3/xxh128/md5/sha1/CRC-64 (ECMA, ISO) of the raw, header or body bytes. `build` writes 0. |
| 0x0C, 0x14, 0x18, 0x24 | u32 0 (C) |
| 0x10 | u32 type id. Equals the type of the entry's first resource (C, 8131/8131 sampled). |
| 0x1C, 0x20 | i32 next, i32 prev: a linked list in index order across all tables (-1 at the ends) (C) |
| 0x28 | u32 build time (unix). Base archives date from 2017-11-30, patch_01 from 2018-11 and patch_02 from 2019-06. |
| 0x2C | name, 128 bytes, NUL-padded. Equals the first resource's name, or `File_0x<id>` when that name is empty (C). |
| 0xAC | 5 x u32: (4,0,0,4,0), or (2,0,0,2,1) for GlobalMetaFile (C values, G meaning) |

Every archive has two special entries. Index 0 is **GlobalMetaFile** (id 0x10, raw and uncompressed, 436 B to
24 KB). It holds a type list and an archive GUID; the GUID is unique per archive. The last index is
**PrefetchingFileInfos** (id 0x91; see below).

### Datafiles (every other entry)

An entry is two compressed **blocks**: a header block and then a body block (C: every one of the 165k entries,
and it is always exactly 2). A block looks like this:

```
u64 0x1004FA9957FBAA33, u16 version = 2, u8 codec, u32 max chunk (0x40000) | 0x80000000 flag
flag set (datafiles):     u32 n, n x (u32 uncompressed, u32 stored), then n x (u32 checksum, stored bytes)
flag clear (prefetch):    repeated to the entry's end: u8 1, u32 stored, u32 uncompressed, u32 checksum, bytes
```

- The codec field is 5 in every datafile, and those chunks are **zstd** frames (C). The frames carry the
  content size and no checksum. No zstd level of `zstandard` 0.25 reproduces them byte for byte, which
  doesn't matter for the game. The codec field is 0 in the prefetch list, whose chunks are **LZO1X** (C).
- A chunk whose stored size equals its uncompressed size is stored as is (C).
- The checksum is **Adler-32 of the stored bytes, started at 0** instead of 1, i.e. `zlib.adler32(data, 0)`
  (C, every chunk).
- Chunks hold at most 0x40000 bytes of uncompressed data (C).

Header block, decompressed: u16 n, then n records `u64 resource id, u32 size, u16 k, k x u16`. The k u16s are
G: unknown; k is 0 in ~97% of sampled records, otherwise mostly 1. The sizes add up to the body length (C). The first
record is always the entry itself (C).

Body block, decompressed: the n resources, one after another. Each is:

```
u32 type, u32 payload size, u32 name length, name, u8 flag,
[flag 1: `pre` bytes, as many as the header record's size leaves over: 7 + 12k seen (G: some object table)],
payload  (starts with u64 resource id, u32 type; C)
```

The flag is 1 on, for example, the animation resources (type 0x0FA3067F) inside `STP_F_BaseJump`, each with
55 `pre` bytes. It is 0 on most resources (C: the layout holds for every resource of every archive).

A datafile carries its dependencies inline. For example, `..._MapDesc` contains the MapDesc *and* its
TextureMap, while the TextureMap also exists as its own entry. The same resource therefore lives in several
datafiles (C). Resource names are plaintext, so nested resources can be named.

Type ids, named after the entry names that carry them (G):

| type | count | names look like |
|---|---|---|
| 0xAC2BBF68 | 106,696 | `Cell07545_DataBlock` (world cells) |
| 0xA2B7E917 | 18,433 | `*_Map`, `*_DiffuseMap`, `*_NormalMap` (textures; a 256x256 icon's payload is 124 header bytes + 64 KiB, which looks like one BCn mip) |
| 0x1D4B87A3 | 12,965 | `*_Mip0`, `*_Mip1` (separately streamed top mips) |
| 0x415D9568 | 8,718 | `*_LOD0..3` (meshes) |
| 0xB22B3E61 | 5,787 | `... Merged Shape`, `MeshShape` |
| 0x85C817C3 | 4,799 | `*_BP`, `*_T01` (materials?) |
| 0x989DC6B2 | 4,777 | `*_MapDesc` |
| 0xD70E6670 | 314 | `*_Set` |

`forge.py info` prints the full list for an archive.

### PrefetchingFileInfos (C)

This entry is one LZO1X stream block. Decompressed it is: u32 n, n x (u64 id, u32 size, u32 offset), u32 blob
length, blob. It lists **every datafile** of the archive, sorted by id; the metadata entry and the prefetch
list itself are left out. Most blobs are 4 zero bytes. The others are 4 + 11k bytes: `u16, u16 k, k x (u64 id,
3 bytes)`, a list of the **entries the datafile depends on** (C on jacket materials: exactly the 4 textures of
the material's TextureSet plus the badge atlas and maps; G in general, and the u16 and the 3 bytes are unknown).
Dependencies can point into other archives: `DataPC_extra_chr` -> `_onboarding` 62 times, `DataPC_STP_Alpes`
-> the character archives 35 times, and 974 Alpes dependencies are ids in no archive at all. `build` copies each
id's blob from the newest archive in the stack that has it, and writes 4 zero bytes for a new id. An unpacked
folder whose `datafile.json` has a `prefetch` key (hex) brings its own blob instead (`swap` writes one). Our
builder reproduces `DataPC_extra_patch_02`'s list byte for byte (decompressed).

## How the patches override the base

The evidence below comes from `forge.py stack` over all 9 base archives.

- A patch archive holds only **changed and new entries**, keyed by the same 64-bit **entry id** (C). Its
  entries keep their base names; a handful were renamed, e.g. `STP_M_Paraglider_001_BP` became
  `STP_M_Paraglide_001_BP`. Their data and build times are newer.
- Patches are **deltas, not cumulative** (C). `DataPC_extra_patch_02`'s 16 entries override the base
  directly: 14 of them were never in `patch_01`. `Game Bootstrap Settings` (0x800) is in all three, with
  build times 2017-11-30, 2018-11-05 and 2019-06-06.
- Patches also add ids (C). For example, `DataPC_extra_2_patch_02` adds 696 ids that are in neither earlier
  archive; Christmas/Halloween gear appears this way.
- **The newest copy wins per id: patch_02, then patch_01, then the base** (C, observed in the live game).
  `powder_asset_trace.log` in the game folder logs `time file offset length` for the game's file reads
  during a session on 2026-10-26. In it, the game mounts `DataPC_STP_Alpes`, then `_patch_01`, then
  `_patch_02`. For each it reads the header (0, 0x41A, 0x446), the index and name tables, the chained table
  headers, the GlobalMetaFile and the whole prefetch list. 391 entries of at least 256 KiB were read in full,
  and **every one came from the archive holding the newest copy**: 258 from the base, 121 from patch_01 (all
  121 ids also exist in the base) and 12 from patch_02 (11 also exist in older archives). The game read 0
  stale copies.
- `streaminginstall.ini` names only the base archives, so the engine must find `<base>_patch_NN.forge` by
  name. Whether it would probe a `_patch_03` is unknown (G). The trace logs reads, not failed opens, and the
  executable is VMProtect-packed, so its loader strings are not readable on disk.
- Ids repeat across *different* base archives as well, e.g. 16,937 ids sit in all three map archives. Each
  map loads its own forge.

## Shipping a modded entry: feasibility

The format side is solved. `extract`, edit a payload, `pack`, then `build --like <base>` produces an archive
that parses back cleanly: two zstd blocks per datafile, Adler-32 (start 0), the 5000-slot tables, the folder
region, a copied GlobalMetaFile with a fresh GUID, and a generated prefetch list. It was tested by rebuilding
`DataPC_extra_patch_02` from its own extracted entries. The file header, table header, folder region,
decompressed prefetch list and every entry's bytes came out identical. The name records were identical except
for the unknown hash, the timestamp and the size field (the original's prefetch list sits in a 280-byte slot).
A sample of 175 datafiles from three archives also rebuilds to the same decompressed content.

The game has never loaded any of this. To get a modded entry in, in order of preference:

1. **`<base>_patch_03.forge`**: a new archive with only the modded ids. It works only if the engine probes
   past `_patch_02`, which is unknown (see the open questions). It costs nothing to try. Through powder-wrap
   the file can be mounted into the session's private mount namespace, so the install stays untouched.
2. **A replacement `<base>_patch_02.forge`**: the original's entries plus the modded ones. Build it with
   `build --keep-guid` from the extracted `_patch_02` datafiles: leave out the ones you replace and add the
   modded datafiles, including ids that so far live only in the base or patch_01. Then bind-mount it over the
   original for the session only. This relies only on the patch mechanism the game was seen using.
3. Patch an archive in place (append the data, repoint the index slot): the same as 2, but it modifies files;
   avoid this.

What a modder must watch:
- Resources are duplicated inline. To retexture something, replace the resource in **every datafile that
  embeds it**. `show` lists a datafile's resources; search by resource id.
- Payload formats: textures are decoded (TEXTURES.md, texture.py); meshes are not yet. Same-size, same-format swaps are the safe start.
- The name-record hash is unknown; if the engine checks it, our archives fail. Ubisoft Connect's "verify
  files" would flag a replaced file, but a bind mount in the session namespace never touches the real one.

### Swapping an entry for another (`swap`)

`swap BASE ENTRY --with SRC_BASE SRC_ENTRY -o DIR` writes an unpacked folder, `DIR/<id>_<name>/`. The folder
holds ENTRY's id and name but SRC_ENTRY's content. Both entries are taken from the newest archive of their
stacks. Feed the folder to `build`. The swap does this:

- The first resource gets ENTRY's id and name. Its **nested object ids** are retargeted too: a resource's
  sub-objects carry ids of their own, serialized as u64 id + u32 class hash. A jacket material has 6, its id
  + 1..4 and two allocated elsewhere. When both payloads have the same length, every place where the two hold
  different id-sized u64s (2^32..2^42) followed by the same class-hash-like u32 maps SRC's id to ENTRY's. Floats
  and other parameters that differ are left alone: they are not followed by a class hash. For payloads of
  different lengths, only the resource id is retargeted. `datafile.json` lists the mapping under `swap.ids`.
- The other resources (e.g. the material's TextureSet) come along unchanged, under their own ids. The tool warns
  if one of them refers to a retargeted id.
- SRC's prefetch blob goes into `datafile.json` as `prefetch`, so the game is told to prefetch the new
  dependencies, not the old ones.
- Different entry types are refused (`--force` overrides).

This is meant for swaps within one family: another colourway's material of the same mesh, for example. That is
what `mods/model_swap/` does, and its README shows how an item in the save leads to its material through the
character template. Checked on 3 jacket materials (1242 bytes each): the results equal the source except for
the 7 retargeted ids, and the archives built from them pass `verify --decompress`. The game has not loaded them
yet.

## Open questions

- Does the engine load `_patch_03` (or `_patch_NN` in general)? This needs a live test. Search a memory dump
  of the unpacked `steep.exe` for `_patch_`, or put a harmless `_patch_03` in a powder-wrap session and check
  whether the asset trace shows reads of it.
- The name record's u64 hash at +4: its algorithm, and whether the engine verifies it.
- The meaning of the datafile header's `extra` u16s, the resource `pre` section (flag 1), and the prefetch
  blobs' leading u16 and 3 bytes per dependency.
- The rest of the GlobalMetaFile (the type list?), and whether a copied one with a new GUID is accepted.
- Payload formats: TextureMap + Mip are decoded (TEXTURES.md); Mesh is not. They are needed for real
  asset replacement (e.g. a DDS to TextureMap converter).
- The folder table's fields, and the 5 u32s at name record +0xAC.
- The writer handles a single table (≤ 4998 modded entries). Chained tables are read but not written.
