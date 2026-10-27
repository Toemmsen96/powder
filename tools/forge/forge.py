#!/usr/bin/env python3
"""Steep's .forge archives (Ubisoft AnvilNext / "Scimitar" engine): list, extract, verify, compare, build.

The groundwork for asset mods: read an archive's index, pull an entry out raw or decompressed (datafiles are
zstd-chunked, the prefetch list is LZO1X), split a datafile into its resources, see which of the base and its
_patch_NN archives provides each entry id, build datafiles/archives back (format-correct as far as the
reader and the shipped archives go; not yet tried in the game), and swap an entry's content for another's under
its own id. The format, and what is confirmed vs guessed, is in README.md next to this file.

Archives are opened read-only and read by seeking (never whole); only `extract`, `pack`, `build` and `swap` write,
to the paths given. Needs Python 3.10+ and `zstandard` or `pyzstd` for zstd; LZO1X is built in.
"""

from __future__ import annotations

import argparse
import datetime
import functools
import glob
import json
import os
import re
import struct
import sys
import time
import uuid
import zlib
from dataclasses import dataclass

FORGE_MAGIC = b"scimitar\x00"
FORGE_VERSION = 27
DATAFILE_MAGIC = 0x1004FA9957FBAA33
TABLE_SLOTS = 5000                      # index/name slots per table (fixed-size tables)
INDEX_REC = struct.Struct("<QQI")       # data offset, entry id, size
NAME_REC_SIZE = 192
TABLE_HDR = struct.Struct("<IIQqIIQQ")  # count, folders, index off, next table, first, last, names off, folders off
ARCHIVE_HDR = struct.Struct("<IIQIqIIQ")  # count, 2, 0, 0, -1, slots, tables, first table
BLOCK_HDR = struct.Struct("<QHBI")      # magic, version (2), codec, max chunk size | layout flag
TABLE_LAYOUT = 0x80000000               # set: chunk size table up front; clear: chunks stream (prefetch list)
MAX_CHUNK = 0x40000
CODEC_LZO1X = 0
CODEC_ZSTD = 5
CODEC_NAMES = {CODEC_LZO1X: "lzo1x", CODEC_ZSTD: "zstd"}
META_ID, PREFETCH_ID = 0x10, 0x91       # GlobalMetaFile (index 0), PrefetchingFileInfos (last index)
GUID_RE = re.compile(rb"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")

# Entry/resource type ids seen in Steep, named after the entry names that carry them (guesses, see README).
TYPE_HINTS = {
    0xAC2BBF68: "DataBlock", 0xA2B7E917: "TextureMap", 0x1D4B87A3: "Mip", 0x415D9568: "Mesh(LOD)",
    0xB22B3E61: "Shape", 0x85C817C3: "Material?", 0x989DC6B2: "MapDesc", 0x51DC6B80: "Entity?",
    0xD70E6670: "TextureSet", 0x6E3C9C6F: "Localization?", 0xE5A83560: "BootstrapSettings",
}


# --- compression ----------------------------------------------------------------------------------------------

def adler32_0(data: bytes) -> int:
    """The chunk checksum: Adler-32 started from 0 instead of 1 (confirmed on every chunk checked)."""
    return zlib.adler32(data, 0)


@functools.cache
def _zstd():
    try:
        import zstandard
        return (lambda d, n: zstandard.ZstdDecompressor().decompress(d, max_output_size=n),
                lambda d, lvl: zstandard.ZstdCompressor(level=lvl).compress(d))
    except ImportError:
        pass
    try:
        import pyzstd
        return (lambda d, n: pyzstd.decompress(d), lambda d, lvl: pyzstd.compress(d, lvl))
    except ImportError:
        sys.exit("forge: zstd needs the `zstandard` or `pyzstd` module")


def lzo1x_decompress(src: bytes, out_len: int) -> bytes:
    """LZO1X (as lzo1x_decompress_safe): used by the PrefetchingFileInfos entry."""
    out = bytearray()
    ip, state = 0, 0  # state: literals after the last match (0..3), 4 after a literal run

    def ext(base: int) -> int:
        nonlocal ip
        t = 0
        while src[ip] == 0:
            t += 255
            ip += 1
        t += base + src[ip]
        ip += 1
        return t

    if src[0] > 17:
        t = src[0] - 17
        out += src[1:1 + t]
        ip = 1 + t
        state = 4 if t >= 4 else t
    while True:
        t = src[ip]
        ip += 1
        if t < 16:
            if state == 0:
                n = (ext(15) if t == 0 else t) + 3
                out += src[ip:ip + n]
                ip += n
                state = 4
                continue
            dist = 1 + (t >> 2) + (src[ip] << 2) + (0x800 if state == 4 else 0)
            ln = 3 if state == 4 else 2
            ip += 1
        elif t >= 64:
            dist = 1 + ((t >> 2) & 7) + (src[ip] << 3)
            ln = (t >> 5) + 1
            ip += 1
        elif t >= 32:
            ln = (ext(31) if t & 31 == 0 else t & 31) + 2
            dist = 1 + ((src[ip] | src[ip + 1] << 8) >> 2)
            ip += 2
        else:
            ln = (ext(7) if t & 7 == 0 else t & 7) + 2
            dist = ((t & 8) << 11) + ((src[ip] | src[ip + 1] << 8) >> 2)
            ip += 2
            if dist == 0:
                break
            dist += 0x4000
        start = len(out) - dist
        if start < 0:
            raise ValueError("lzo1x: match before start of output")
        for i in range(ln):
            out.append(out[start + i])
        state = src[ip - 2] & 3
        out += src[ip:ip + state]
        ip += state
    if len(out) != out_len:
        raise ValueError(f"lzo1x: {len(out)} bytes, expected {out_len}")
    return bytes(out)


def lzo1x_store(data: bytes) -> bytes:
    """A valid LZO1X stream that holds `data` as one literal run (no compression; enough for small lists)."""
    n = len(data)
    if n == 0:
        return b"\x11\x00\x00"
    if n <= 238:
        head = bytes([17 + n])
    else:
        rest = n - 18
        zeros = (rest - 1) // 255
        head = b"\x00" + b"\x00" * zeros + bytes([rest - 255 * zeros])
    return head + data + b"\x11\x00\x00"


def decompress(codec: int, data: bytes, usize: int) -> bytes:
    if len(data) == usize:  # stored: compression did not pay off
        return data
    if codec == CODEC_ZSTD:
        out = _zstd()[0](data, usize)
    elif codec == CODEC_LZO1X:
        out = lzo1x_decompress(data, usize)
    else:
        raise ValueError(f"unknown codec {codec}")
    if len(out) != usize:
        raise ValueError(f"decompressed {len(out)} bytes, expected {usize}")
    return out


# --- compressed blocks and datafiles --------------------------------------------------------------------------

@dataclass
class Chunk:
    usize: int
    csize: int
    checksum: int
    data: bytes


@dataclass
class Block:
    version: int
    codec: int
    max_chunk: int
    table_layout: bool
    chunks: list[Chunk]

    def decompress(self) -> bytes:
        return b"".join(decompress(self.codec, c.data, c.usize) for c in self.chunks)

    def bad_checksums(self) -> int:
        return sum(adler32_0(c.data) != c.checksum for c in self.chunks)


def parse_block(data: bytes, pos: int = 0) -> tuple[Block, int]:
    magic, version, codec, mc = BLOCK_HDR.unpack_from(data, pos)
    if magic != DATAFILE_MAGIC:
        raise ValueError(f"no block magic at {pos:#x}")
    pos += BLOCK_HDR.size
    chunks = []
    if mc & TABLE_LAYOUT:
        (n,) = struct.unpack_from("<I", data, pos)
        sizes = [struct.unpack_from("<II", data, pos + 4 + 8 * i) for i in range(n)]  # (uncompressed, stored)
        pos += 4 + 8 * n
        for us, cs in sizes:
            (ck,) = struct.unpack_from("<I", data, pos)
            chunks.append(Chunk(us, cs, ck, data[pos + 4:pos + 4 + cs]))
            pos += 4 + cs
    else:
        while pos < len(data):  # u8 1, u32 stored, u32 uncompressed, u32 checksum, data; up to the entry's end
            one, cs, us, ck = struct.unpack_from("<BIII", data, pos)
            if one != 1:
                raise ValueError(f"stream chunk marker {one} at {pos:#x}")
            chunks.append(Chunk(us, cs, ck, data[pos + 13:pos + 13 + cs]))
            pos += 13 + cs
    for c in chunks:
        if len(c.data) != c.csize:
            raise ValueError("chunk runs past the entry")
    return Block(version, codec, mc & ~TABLE_LAYOUT, bool(mc & TABLE_LAYOUT), chunks), pos


def build_block(raw: bytes, codec: int = CODEC_ZSTD, level: int = 9, table_layout: bool = True) -> bytes:
    pieces = [raw[i:i + MAX_CHUNK] for i in range(0, len(raw), MAX_CHUNK)] or [b""]
    chunks = []
    for p in pieces:
        if codec == CODEC_ZSTD:
            c = _zstd()[1](p, level)
        elif codec == CODEC_LZO1X:
            c = lzo1x_store(p)
        else:
            raise ValueError(f"unknown codec {codec}")
        if codec == CODEC_ZSTD and len(c) >= len(p):
            c = p
        chunks.append((len(p), c))
    out = bytearray(BLOCK_HDR.pack(DATAFILE_MAGIC, 2, codec, MAX_CHUNK | (TABLE_LAYOUT if table_layout else 0)))
    if table_layout:
        out += struct.pack("<I", len(chunks))
        for us, c in chunks:
            out += struct.pack("<II", us, len(c))
        for us, c in chunks:
            out += struct.pack("<I", adler32_0(c)) + c
    else:
        for us, c in chunks:
            out += struct.pack("<BIII", 1, len(c), us, adler32_0(c)) + c
    return bytes(out)


@dataclass
class Resource:
    """One object in a datafile: header record (id, size, extra u16s) + body part.

    Body part: u32 type, u32 payload size, u32 name length, name, u8 flag, `pre` (only with flag 1; its length
    is what the header record's size leaves over), payload (starts with the resource's own u64 id, u32 type).
    """
    id: int
    type: int
    name: str
    payload: bytes
    flag: int = 0
    pre: bytes = b""
    extra: tuple = ()

    def body(self) -> bytes:
        name = self.name.encode("latin1")
        return (struct.pack("<III", self.type, len(self.payload), len(name)) + name + bytes([self.flag]) + self.pre
                + self.payload)


@dataclass
class Datafile:
    header_block: Block
    body_block: Block
    resources: list[Resource]


def is_datafile(data: bytes) -> bool:
    return len(data) >= 8 and struct.unpack_from("<Q", data)[0] == DATAFILE_MAGIC


def parse_datafile(data: bytes) -> Datafile:
    hb, pos = parse_block(data, 0)
    bb, pos = parse_block(data, pos)
    if pos != len(data):
        raise ValueError(f"{len(data) - pos} bytes after the second block")
    header, body = hb.decompress(), bb.decompress()
    (n,) = struct.unpack_from("<H", header)
    p, q, res = 2, 0, []
    for _ in range(n):
        rid, size, k = struct.unpack_from("<QIH", header, p)
        extra = struct.unpack_from(f"<{k}H", header, p + 14)
        p += 14 + 2 * k
        typ, psz, nl = struct.unpack_from("<III", body, q)
        flag = body[q + 12 + nl]
        npre = size - (13 + nl + psz)
        if npre < 0 or (npre and flag != 1):
            raise ValueError(f"resource {rid:#x}: size {size}, name {nl}, payload {psz}, flag {flag} do not fit")
        name = body[q + 12:q + 12 + nl].decode("latin1")
        pre = body[q + 13 + nl:q + 13 + nl + npre]
        res.append(Resource(rid, typ, name, body[q + 13 + nl + npre:q + size], flag, pre, extra))
        q += size
    if p != len(header) or q != len(body):
        raise ValueError("datafile header/body sizes do not add up")
    return Datafile(hb, bb, res)


def build_datafile(resources: list[Resource], level: int = 9) -> bytes:
    header = bytearray(struct.pack("<H", len(resources)))
    bodies = []
    for r in resources:
        b = r.body()
        bodies.append(b)
        header += struct.pack("<QIH", r.id, len(b), len(r.extra)) + struct.pack(f"<{len(r.extra)}H", *r.extra)
    return build_block(bytes(header), level=level) + build_block(b"".join(bodies), level=level)


# --- same-size datafiles (mods/world_editor's live edits) ------------------------------------------------------

ZSTD_FRAME_MAGIC = b"\x28\xb5\x2f\xfd"


def zstd_frame_header_size(frame: bytes) -> int:
    """Bytes before a zstd frame's first block: magic, descriptor, window descriptor, dictionary id, content size."""
    if frame[:4] != ZSTD_FRAME_MAGIC or len(frame) < 6:
        raise ValueError("not a zstd frame")
    fhd = frame[4]
    single = fhd >> 5 & 1
    fcs = (0 if not single else 1, 2, 4, 8)[fhd >> 6]
    return 5 + (0 if single else 1) + (0, 1, 2, 4)[fhd & 3] + fcs


def zstd_padding(d: int) -> bytes | None:
    """`d` bytes of empty zstd blocks (raw, size 0: 3 bytes; RLE, size 0: 4 bytes), none of them the last: inserted
    before a frame's first block they decompress to nothing. None for 1, 2 and 5 (no sum of 3s and 4s)."""
    for rle in range(4):
        if d - 4 * rle >= 0 and (d - 4 * rle) % 3 == 0:
            return b"\x02\x00\x00\x00" * rle + b"\x00\x00\x00" * ((d - 4 * rle) // 3)
    return None


def pad_datafile(data: bytes, size: int) -> bytes | None:
    """`data` (a datafile) made exactly `size` bytes long without changing what it decompresses to: empty zstd blocks
    go into one of its compressed chunks (chunk size and checksum updated). The datafile stays a well-formed datafile
    of two blocks (no trailing bytes), so a reader that expects exactly the entry's size reads it as it is. None if
    it cannot be done (bigger than `size`, 1, 2 or 5 bytes short, no zstd chunk); try another compression level."""
    d = size - len(data)
    if d == 0:
        return data
    if d < 0 or zstd_padding(d) is None:
        return None
    blocks, pos = [], 0
    for _ in range(2):
        b, end = parse_block(data, pos)
        if not b.table_layout or b.codec != CODEC_ZSTD:
            return None
        blocks.append(b)
        pos = end
    # spread over the compressed chunks, biggest room first; a padded chunk stays shorter than its data (equal
    # would read as "stored"; longer than its data no shipped chunk is)
    rooms = sorted(((c.usize - c.csize - 1, k, j) for k, b in enumerate(blocks) for j, c in enumerate(b.chunks)
                    if c.csize < c.usize and c.data[:4] == ZSTD_FRAME_MAGIC), reverse=True)
    share: dict[tuple[int, int], int] = {}
    for room, k, j in rooms:
        if d == 0:
            break
        t = min(d, room)
        while t > 0 and (zstd_padding(t) is None or 0 < d - t < 6):
            t -= 1
        if t > 0:
            share[(k, j)] = t
            d -= t
    if d:
        return None
    out = bytearray()
    for k, b in enumerate(blocks):
        chunks = list(b.chunks)
        for j, c in enumerate(chunks):
            if (k, j) in share:
                h = zstd_frame_header_size(c.data)
                new = c.data[:h] + zstd_padding(share[(k, j)]) + c.data[h:]
                chunks[j] = Chunk(c.usize, len(new), adler32_0(new), new)
        out += BLOCK_HDR.pack(DATAFILE_MAGIC, b.version, b.codec, b.max_chunk | TABLE_LAYOUT)
        out += struct.pack("<I", len(chunks))
        for c in chunks:
            out += struct.pack("<II", c.usize, c.csize)
        for c in chunks:
            out += struct.pack("<I", c.checksum) + c.data
    out = bytes(out)
    if len(out) != size:
        raise ValueError(f"internal error: padded to {len(out)}, not {size}")
    return out


def fit_datafile(resources: list[Resource], size: int, levels=(9, 19, 22)) -> tuple[bytes, bool]:
    """The resources as a datafile of exactly `size` bytes if any compression level gets them there (True), else
    the smallest datafile tried (False: it needs more room than `size`)."""
    best = None
    for lvl in levels:
        data = build_datafile(resources, level=lvl)
        if len(data) <= size:
            padded = pad_datafile(data, size)
            if padded is not None:
                return padded, True
        if best is None or len(data) < len(best):
            best = data
    return best, False


# --- archives -------------------------------------------------------------------------------------------------

@dataclass
class Entry:
    index: int
    id: int
    offset: int
    size: int
    alloc: int       # name record's first u32: the size too, except where the writer reused a bigger slot
    hash: int        # name record +4: changes with every build of an entry; not reproduced (unknown hash)
    type: int
    timestamp: int
    name: str
    tail: tuple      # name record +0xac: (4,0,0,4,0); (2,0,0,2,1) for GlobalMetaFile


@dataclass
class Table:
    offset: int
    count: int
    folders: int
    index_offset: int
    next: int
    first: int
    last: int
    names_offset: int
    folders_offset: int


class Forge:
    def __init__(self, path: str):
        self.path = path
        self.f = open(path, "rb")
        if self.f.read(9) != FORGE_MAGIC:
            raise ValueError(f"{path}: not a .forge (no 'scimitar' magic)")
        self.version, self.header_offset, self.unk_a, self.unk_b = struct.unpack("<IQQQ", self.f.read(28))
        self.f.seek(self.header_offset)
        (self.count, self.unk_c, _, _, _, self.slots, ntables,
         first) = ARCHIVE_HDR.unpack(self.f.read(ARCHIVE_HDR.size))
        self.tables: list[Table] = []
        self.entries: list[Entry] = []
        off = first
        for _ in range(ntables):
            self.f.seek(off)
            t = Table(off, *TABLE_HDR.unpack(self.f.read(TABLE_HDR.size)))
            self.tables.append(t)
            self.f.seek(t.index_offset)
            idx = self.f.read(INDEX_REC.size * t.count)
            self.f.seek(t.names_offset)
            names = self.f.read(NAME_REC_SIZE * t.count)
            for i in range(t.count):
                o, eid, size = INDEX_REC.unpack_from(idx, INDEX_REC.size * i)
                n = names[NAME_REC_SIZE * i:NAME_REC_SIZE * (i + 1)]
                alloc, h = struct.unpack_from("<IQ", n)
                typ, ts = struct.unpack_from("<I", n, 0x10)[0], struct.unpack_from("<I", n, 0x28)[0]
                name = n[0x2C:0xAC].split(b"\0")[0].decode("latin1")
                self.entries.append(Entry(len(self.entries), eid, o, size, alloc, h, typ, ts, name,
                                          struct.unpack_from("<5I", n, 0xAC)))
            if t.next == -1:
                break
            off = t.next
        if len(self.entries) != self.count:
            raise ValueError(f"{path}: header says {self.count} entries, tables hold {len(self.entries)}")
        self.by_id = {e.id: e for e in self.entries}

    def read(self, e: Entry) -> bytes:
        self.f.seek(e.offset)
        return self.f.read(e.size)

    def find(self, spec: str, exact: bool = False) -> list[Entry]:
        """Entries by id, exact name, or (unless `exact`) name substring."""
        if re.fullmatch(r"0x[0-9a-fA-F]+|\d+", spec):
            e = self.by_id.get(int(spec, 0))
            if e:
                return [e]
        named = [e for e in self.entries if e.name == spec]
        return named or ([] if exact else [e for e in self.entries if spec.lower() in e.name.lower()])


def fmt_time(ts: int) -> str:
    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%Y-%m-%d")


def fmt_type(t: int) -> str:
    hint = TYPE_HINTS.get(t)
    return f"{t:#010x}" + (f" {hint}" if hint else "")


def safe_name(s: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]+", "_", s)[:80] or "unnamed"


def patch_stack(base: str) -> list[str]:
    """The base archive and its _patch_NN siblings, lowest first (the order the game presumably applies)."""
    stem = base[:-len(".forge")] if base.endswith(".forge") else base
    patches = [p for p in glob.glob(glob.escape(stem) + "_patch_*.forge")
               if re.fullmatch(r"_patch_\d+\.forge", p[len(stem):])]
    return [stem + ".forge"] + sorted(patches, key=lambda p: int(re.search(r"_patch_(\d+)\.forge$", p)[1]))


def prefetch_records(raw: bytes) -> tuple[list[tuple[int, int, int]], bytes]:
    """PrefetchingFileInfos (decompressed): u32 n, n x (u64 id, u32 size, u32 offset), u32 blob size, blob."""
    (n,) = struct.unpack_from("<I", raw)
    recs = [struct.unpack_from("<QII", raw, 4 + 16 * i) for i in range(n)]
    (blob,) = struct.unpack_from("<I", raw, 4 + 16 * n)
    return recs, raw[8 + 16 * n:8 + 16 * n + blob]


def build_prefetch(blobs: dict[int, bytes]) -> bytes:
    """A prefetch list over these entry ids (sorted, as shipped); an id without a blob gets 4 zero bytes."""
    ids = sorted(blobs)
    parts = [blobs[i] or bytes(4) for i in ids]
    raw = bytearray(struct.pack("<I", len(ids)))
    off = 0
    for i, p in zip(ids, parts):
        raw += struct.pack("<QII", i, len(p), off)
        off += len(p)
    raw += struct.pack("<I", off) + b"".join(parts)
    return build_block(bytes(raw), codec=CODEC_LZO1X, table_layout=False)


def read_prefetch(fg: Forge) -> dict[int, bytes]:
    e = fg.by_id.get(PREFETCH_ID)
    if not e:
        return {}
    recs, blob = prefetch_records(parse_block(fg.read(e))[0].decompress())
    return {rid: blob[off:off + size] for rid, size, off in recs}


# --- archive writer -------------------------------------------------------------------------------------------

def folder_table(count: int) -> bytes:
    """The folder region as every shipped archive has it (only the root's file count varies: entries - 1)."""
    t = bytearray(TABLE_SLOTS * 176 + 1)
    struct.pack_into("<II", t, 0, count - 1, 1)
    t[0x08:0x14] = b"\xff" * 12
    struct.pack_into("<I", t, 0x94, 13)
    struct.pack_into("<II", t, 0xA8, 4, 1)
    t[0xB0:0xC0] = b"\xff" * 16
    t[0xC4:0xC4 + 11] = b"_Lost&Found"
    struct.pack_into("<I", t, 0x144, 14)
    struct.pack_into("<I", t, 0x14C, 13)
    struct.pack_into("<II", t, 0x158, 4, 1)
    return bytes(t)


def write_forge(path: str, items: list[tuple[int, int, str, bytes]], meta: bytes,
                prefetch_blobs: dict[int, bytes] | None = None, timestamp: int | None = None):
    """Write an archive: GlobalMetaFile, `items` (id, type, name, datafile bytes), PrefetchingFileInfos.

    Layout copied from the shipped patch archives (one table of 5000 slots, folder region, data). The name
    record's +4 hash is unknown and written as 0.
    """
    ts = int(time.time()) if timestamp is None else timestamp
    prefetch = build_prefetch({i: (prefetch_blobs or {}).get(i, b"") for i, *_ in items})
    ents = [(META_ID, 0, "GlobalMetaFile", meta)] + list(items) + [(PREFETCH_ID, 0, "PrefetchingFileInfos", prefetch)]
    n = len(ents)
    if n > TABLE_SLOTS:
        raise ValueError(f"{n} entries: more than one table is not supported by the writer")
    if len({e[0] for e in ents}) != n:
        raise ValueError("duplicate entry ids")
    hdr_off, tbl_off = 0x41A, 0x446
    idx_off = tbl_off + TABLE_HDR.size
    names_off = idx_off + INDEX_REC.size * TABLE_SLOTS
    fold_off = names_off + NAME_REC_SIZE * TABLE_SLOTS
    data_off = fold_off + TABLE_SLOTS * 176 + 1
    order = [n - 1] + list(range(1, n - 1)) + [0]  # data: prefetch first, meta last (as in the patches)
    offsets, pos = {}, data_off
    for k in order:
        offsets[k] = pos
        pos += len(ents[k][3])
    head = bytearray(hdr_off)
    head[0:9] = FORGE_MAGIC
    struct.pack_into("<IQQQ", head, 9, FORGE_VERSION, hdr_off, 16, 1)
    head += ARCHIVE_HDR.pack(n, 2, 0, 0, -1, TABLE_SLOTS, 1, tbl_off)
    head += TABLE_HDR.pack(n, 2, idx_off, -1, 0, TABLE_SLOTS - 1, names_off, fold_off)
    index = bytearray(INDEX_REC.size * TABLE_SLOTS)
    names = bytearray(NAME_REC_SIZE * TABLE_SLOTS)
    for k, (eid, typ, name, data) in enumerate(ents):
        INDEX_REC.pack_into(index, INDEX_REC.size * k, offsets[k], eid, len(data))
        rec = bytearray(NAME_REC_SIZE)
        struct.pack_into("<IQ", rec, 0, len(data), 0)
        struct.pack_into("<I", rec, 0x10, typ)
        struct.pack_into("<iiII", rec, 0x1C, k + 1 if k + 1 < n else -1, k - 1, 0, ts)
        nb = name.encode("latin1")[:127]
        rec[0x2C:0x2C + len(nb)] = nb
        struct.pack_into("<5I", rec, 0xAC, *((2, 0, 0, 2, 1) if eid == META_ID else (4, 0, 0, 4, 0)))
        names[NAME_REC_SIZE * k:NAME_REC_SIZE * (k + 1)] = rec
    with open(path, "wb") as f:
        f.write(head + index + names + folder_table(n))
        for k in order:
            f.write(ents[k][3])


# --- commands -------------------------------------------------------------------------------------------------

def cmd_info(a):
    fg = Forge(a.archive)
    print(f"{a.archive}: forge version {fg.version}, {fg.count} entries in {len(fg.tables)} table(s), "
          f"{os.path.getsize(a.archive)} bytes")
    for t in fg.tables:
        print(f"  table @{t.offset:#x}: entries {t.first}..{t.first + t.count - 1} ({t.count}), "
              f"index @{t.index_offset:#x}, names @{t.names_offset:#x}, folders @{t.folders_offset:#x}")
    types: dict[int, list] = {}
    for e in fg.entries:
        types.setdefault(e.type, []).append(e)
    print("  types:")
    for t, es in sorted(types.items(), key=lambda kv: -len(kv[1])):
        print(f"    {fmt_type(t):30} {len(es):6}  e.g. {es[0].name}")
    ts = [e.timestamp for e in fg.entries]
    print(f"  built {fmt_time(min(ts))} .. {fmt_time(max(ts))}")


def cmd_list(a):
    fg = Forge(a.archive)
    for e in fg.entries:
        if a.grep and a.grep.lower() not in e.name.lower():
            continue
        if a.type and e.type != int(a.type, 16):
            continue
        if a.json:
            print(json.dumps({"index": e.index, "id": f"{e.id:#x}", "offset": e.offset, "size": e.size,
                              "type": f"{e.type:#010x}", "time": e.timestamp, "name": e.name}))
        else:
            print(f"{e.id:#018x}  off={e.offset:#012x}  size={e.size:9}  {fmt_type(e.type):24} "
                  f"{fmt_time(e.timestamp)}  {e.name}")


def describe(fg: Forge, e: Entry, out=print):
    out(f"{e.name}  id={e.id:#x}  index={e.index}  offset={e.offset:#x}  size={e.size}  type={fmt_type(e.type)}  "
        f"built={fmt_time(e.timestamp)}  hash={e.hash:#018x}" + (f"  slot={e.alloc}" if e.alloc != e.size else ""))
    data = fg.read(e)
    if not is_datafile(data):
        out(f"  raw (not compressed), starts {data[:16].hex(' ')}")
        return
    if e.id == PREFETCH_ID:
        b, _ = parse_block(data)
        raw = b.decompress()
        recs, blob = prefetch_records(raw)
        out(f"  prefetch list ({CODEC_NAMES.get(b.codec, b.codec)}, {len(b.chunks)} chunk(s), {len(raw)} bytes): "
            f"{len(recs)} ids, blob {len(blob)} bytes")
        for rid, sz, off in recs[:10]:
            nm = fg.by_id[rid].name if rid in fg.by_id else "?"
            out(f"    {rid:#018x}  size={sz:5} off={off:6}  {nm}")
        return
    df = parse_datafile(data)
    for label, b in (("header", df.header_block), ("body", df.body_block)):
        us = sum(c.usize for c in b.chunks)
        cs = sum(c.csize for c in b.chunks)
        out(f"  {label} block: {CODEC_NAMES.get(b.codec, b.codec)}, {len(b.chunks)} chunk(s), {cs} -> {us} bytes, "
            f"bad checksums {b.bad_checksums()}")
    out(f"  {len(df.resources)} resource(s):")
    for r in df.resources:
        ex = (f"  extra={list(r.extra)}" if r.extra else "") + (f"  pre={len(r.pre)}" if r.flag else "")
        out(f"    {r.id:#018x}  {fmt_type(r.type):24} payload={len(r.payload):9}  {r.name or '(no name)'}{ex}")


def cmd_show(a):
    fg = Forge(a.archive)
    for spec in a.entry:
        found = fg.find(spec)
        if not found:
            print(f"{spec}: no such entry", file=sys.stderr)
        for e in found[:a.max]:
            describe(fg, e)


def cmd_extract(a):
    fg = Forge(a.archive)
    sel = list(fg.entries) if a.all else [e for s in a.entry for e in fg.find(s)[:a.max]]
    for s in [] if a.all else a.entry:
        if not fg.find(s):
            print(f"{s}: no such entry", file=sys.stderr)
    os.makedirs(a.out, exist_ok=True)
    for e in sel:
        data = fg.read(e)
        base = os.path.join(a.out, f"{e.id:016x}_{safe_name(e.name)}")
        with open(base + (".datafile" if is_datafile(data) else ".bin"), "wb") as f:
            f.write(data)
        line = f"{e.id:#018x} {e.name}: {len(data)} bytes"
        if is_datafile(data) and not a.raw_only:
            if e.id == PREFETCH_ID:
                with open(base + ".dec", "wb") as f:
                    f.write(parse_block(data)[0].decompress())
            else:
                df = parse_datafile(data)
                write_unpacked(base, e.id, e.name, e.type, df.resources)
                line += f", {len(df.resources)} resource(s) -> {base}/"
        print(line)


def write_unpacked(base: str, eid: int, name: str, typ: int, resources: list[Resource], **more):
    """An unpacked datafile: one .payload per resource + datafile.json (`more` adds top-level keys)."""
    os.makedirs(base, exist_ok=True)
    manifest = []
    for k, r in enumerate(resources):
        fn = f"{k:03d}_{r.id:016x}_{safe_name(r.name)}.payload"
        with open(os.path.join(base, fn), "wb") as f:
            f.write(r.payload)
        manifest.append({"id": f"{r.id:#x}", "type": f"{r.type:#010x}", "name": r.name,
                         "flag": r.flag, "pre": r.pre.hex(), "extra": list(r.extra), "payload": fn})
    with open(os.path.join(base, "datafile.json"), "w") as f:
        json.dump({"entry": f"{eid:#x}", "name": name, "type": f"{typ:#010x}", "resources": manifest, **more},
                  f, indent=1)


def load_unpacked(d: str) -> tuple[dict, list[Resource]]:
    with open(os.path.join(d, "datafile.json")) as f:
        m = json.load(f)
    res = []
    for r in m["resources"]:
        with open(os.path.join(d, r["payload"]), "rb") as f:
            res.append(Resource(int(r["id"], 16), int(r["type"], 16), r["name"], f.read(), r.get("flag", 0),
                                bytes.fromhex(r.get("pre", "")), tuple(r.get("extra", ()))))
    return m, res


def cmd_pack(a):
    _, res = load_unpacked(a.dir)
    data = build_datafile(res, level=a.level)
    parse_datafile(data)  # self-check
    with open(a.out, "wb") as f:
        f.write(data)
    print(f"{a.out}: {len(data)} bytes, {len(res)} resource(s)")


def cmd_stack(a):
    paths = patch_stack(a.base)
    archives = [Forge(p) for p in paths]
    winner: dict[int, int] = {}
    own = [{e.id for e in fg.entries} - {META_ID, PREFETCH_ID} for fg in archives]  # every archive has its own
    for k, ids in enumerate(own):
        for i in ids:
            winner[i] = k
    print("(GlobalMetaFile and PrefetchingFileInfos, which every archive has, left out)")
    for k, fg in enumerate(archives):
        ids = own[k]
        earlier = set().union(*own[:k])
        wins = sum(1 for i in ids if winner[i] == k)
        print(f"{os.path.basename(fg.path)}: {len(ids)} entries, {len(ids & earlier)} override earlier archives, "
              f"{len(ids - earlier)} new, {wins} effective")
    if a.entries or a.grep:
        for i, k in sorted(winner.items(), key=lambda kv: archives[kv[1]].by_id[kv[0]].name):
            e = archives[k].by_id[i]
            if a.grep and a.grep.lower() not in e.name.lower():
                continue
            chain = [os.path.basename(x.path)[:-6] for x in archives if i in x.by_id]
            print(f"{i:#018x}  {e.name:60} <- {' > '.join(reversed(chain))}")


def cmd_verify(a):
    fg = Forge(a.archive)
    bad = 0
    chunks = 0
    for e in fg.entries:
        try:
            data = fg.read(e)
            if len(data) != e.size:
                raise ValueError("entry runs past the end of the file")
            if not is_datafile(data):
                continue
            if e.id == PREFETCH_ID:
                b, _ = parse_block(data)
                blocks = [b]
                if a.decompress:
                    prefetch_records(b.decompress())
            else:
                hb, pos = parse_block(data)
                bb, pos = parse_block(data, pos)
                blocks = [hb, bb]
                if a.decompress:
                    df = parse_datafile(data)
                    if df.resources[0].id != e.id or df.resources[0].type != e.type:
                        raise ValueError("first resource is not the entry (id/type)")
            for b in blocks:
                chunks += len(b.chunks)
                if b.bad_checksums():
                    raise ValueError(f"{b.bad_checksums()} bad chunk checksum(s)")
        except Exception as ex:  # report and go on
            bad += 1
            print(f"{e.id:#018x} {e.name}: {ex}")
    print(f"{a.archive}: {len(fg.entries)} entries, {chunks} chunks checked, {bad} problem(s)")
    return 1 if bad else 0


ID_MIN, ID_MAX = 1 << 32, 1 << 42  # object ids in Steep's data are 35..41 bits; one shifted by a byte is not


def newest_entry(base: str, spec: str) -> tuple[Forge, Entry]:
    """An entry as the game loads it: from the newest archive of `base`'s stack that has it (one match only). An id
    or exact name anywhere in the stack wins over a name substring (a patch's `X_Zippers01` must not shadow the
    base's `X`)."""
    archives = [Forge(p) for p in reversed(patch_stack(base)) if os.path.exists(p)]
    for exact in (True, False):
        for fg in archives:
            found = fg.find(spec, exact)
            if len(found) > 1:
                sys.exit(f"{spec}: {len(found)} entries in {os.path.basename(fg.path)} match; give the id or the "
                         "exact name")
            if found:
                return fg, found[0]
    sys.exit(f"{spec}: in no archive of {base}'s stack")


def remap_ids(src: bytes, dst: bytes, keep: set[int]) -> dict[int, int]:
    """Object ids that `src` holds where the same-layout `dst` holds its own, compared offset by offset.

    A resource's nested objects carry ids of their own (a material's 6 sub-objects: its id + 1..4 and two ids
    allocated elsewhere). Each is serialized as u64 id + u32 class hash, like the resource itself (id + type).
    A pair is taken where both sides hold a u64 in [ID_MIN, ID_MAX), the u64s differ, and the same class-hash-like
    u32 follows (low and high byte nonzero: parameters such as floats or small ints that differ are not ids). Ids
    in `keep` (resources the datafile carries along, e.g. a material's texture set) stay. Contradicting pairs drop.
    """
    pairs: dict[int, int] = {}
    bad: set[int] = set()
    o, end = 0, min(len(src), len(dst)) - 11
    while o < end:
        s, d = struct.unpack_from("<Q", src, o)[0], struct.unpack_from("<Q", dst, o)[0]
        cls = src[o + 8:o + 12]
        if (s != d and s not in keep and ID_MIN <= s < ID_MAX and ID_MIN <= d < ID_MAX and cls == dst[o + 8:o + 12]
                and cls[0] and cls[3]):
            if pairs.get(s, d) != d:
                bad.add(s)
            pairs[s] = d
            o += 8
        else:
            o += 1
    return {s: d for s, d in pairs.items() if s not in bad}


def cmd_swap(a):
    dfg, de = newest_entry(a.base, a.entry)
    sfg, se = newest_entry(*a.src)
    dd, sd = parse_datafile(dfg.read(de)), parse_datafile(sfg.read(se))
    d0, s0 = dd.resources[0], sd.resources[0]
    if d0.type != s0.type and not a.force:
        sys.exit(f"swap: {de.name} is {fmt_type(d0.type)}, {se.name} is {fmt_type(s0.type)} (--force to do it anyway)")
    keep = {r.id for r in sd.resources[1:]}
    idmap = {s0.id: d0.id}
    if len(s0.payload) == len(d0.payload):
        idmap.update(remap_ids(s0.payload, d0.payload, keep | {s0.id}))
    else:
        print(f"  payloads differ in size ({len(s0.payload)} vs {len(d0.payload)}): only the resource id is retargeted")
    payload = s0.payload
    for s, d in idmap.items():
        payload = payload.replace(struct.pack("<Q", s), struct.pack("<Q", d))
        print(f"  id {s:#x} -> {d:#x}")
    for r in sd.resources[1:]:
        if any(struct.pack("<Q", s) in r.payload for s in idmap):
            print(f"  warning: {r.name or hex(r.id)} (carried along unchanged) refers to a retargeted id")
    res = [Resource(d0.id, s0.type, d0.name, payload, s0.flag, s0.pre, s0.extra)] + sd.resources[1:]
    check = parse_datafile(build_datafile(res))
    assert check.resources[0].id == de.id and [r.payload for r in check.resources] == [r.payload for r in res]
    blob = read_prefetch(sfg).get(se.id)
    out = os.path.join(a.out, f"{de.id:016x}_{safe_name(de.name)}")
    more = {"swap": {"entry": f"{de.id:#x}", "entry_archive": os.path.basename(dfg.path), "from": f"{se.id:#x}",
                     "from_name": se.name, "from_archive": os.path.basename(sfg.path),
                     "ids": {f"{s:#x}": f"{d:#x}" for s, d in idmap.items()}}}
    if blob is not None:
        more["prefetch"] = blob.hex()
    write_unpacked(out, de.id, de.name, d0.type, res, **more)
    print(f"{out}/: {de.name} ({os.path.basename(dfg.path)}) now holds {se.name} ({os.path.basename(sfg.path)}), "
          f"{len(res)} resource(s)" + (", its prefetch blob" if blob is not None else ""))


def cmd_build(a):
    stack = [Forge(p) for p in patch_stack(a.like) if os.path.exists(p)]
    meta = stack[-1].read(stack[-1].by_id[META_ID])  # the newest archive's GlobalMetaFile ...
    if not a.keep_guid:  # ... with a GUID of our own (each shipped archive has a different one)
        meta = GUID_RE.sub(str(uuid.uuid4()).encode(), meta, count=1)
    blobs: dict[int, bytes] = {}
    for fg in stack:  # each id's prefetch blob as the newest archive that has it says
        blobs.update(read_prefetch(fg))
    own_blobs: dict[int, bytes] = {}  # ... unless an unpacked folder brings its own (`swap` does)
    items = []
    for p in a.datafiles:
        m = None
        if os.path.isdir(p):
            m, res = load_unpacked(p)
            data = build_datafile(res, level=a.level)
        else:
            with open(p, "rb") as f:
                data = f.read()
        df = parse_datafile(data)
        first = df.resources[0]
        if m and "prefetch" in m:
            own_blobs[first.id] = bytes.fromhex(m["prefetch"])
        items.append((first.id, first.type, first.name or f"File_{first.id:#x}", data))
        known = next((os.path.basename(fg.path) for fg in reversed(stack) if first.id in fg.by_id), "new id")
        print(f"  {first.id:#018x}  {fmt_type(first.type):24} {len(data):9}  {first.name}  ({known})"
              + (", own prefetch blob" if first.id in own_blobs else ""))
    if os.path.realpath(a.out) in {os.path.realpath(fg.path) for fg in stack}:
        sys.exit("build: refusing to overwrite an archive of the stack")
    write_forge(a.out, items, meta, {**blobs, **own_blobs})
    check = Forge(a.out)
    print(f"{a.out}: {check.count} entries ({len(items)} + GlobalMetaFile + PrefetchingFileInfos), "
          f"{os.path.getsize(a.out)} bytes")


def header_ids(fg: Forge, e: Entry) -> list[int]:
    """The resource ids a datafile holds, from its header block alone (usually the entry's first few hundred
    bytes, so a whole archive is scanned without reading its bodies)."""
    fg.f.seek(e.offset)
    head = fg.f.read(min(e.size, 4096))
    try:
        blk, _ = parse_block(head)
    except (ValueError, struct.error):
        blk, _ = parse_block(fg.read(e))
    h = blk.decompress()
    (n,) = struct.unpack_from("<H", h)
    out, p = [], 2
    for _ in range(n):
        rid, _, k = struct.unpack_from("<QIH", h, p)
        out.append(rid)
        p += 14 + 2 * k
    return out


def cmd_where(a):
    paths = []
    for p in a.archives:
        paths += sorted(glob.glob(os.path.join(p, "*.forge"))) if os.path.isdir(p) else [p]
    want = {int(x, 16) for x in a.id}
    hits = 0
    for p in paths:
        fg = Forge(p)
        base = re.sub(r"_patch_\d+$", "", os.path.basename(p)[:-6])
        for e in fg.entries:
            if e.id in (META_ID, PREFETCH_ID):
                continue
            found = want & set(header_ids(fg, e))
            for rid in sorted(found):
                hits += 1
                print(f"{rid:#018x} in {os.path.basename(p)}: entry {e.id:#x} {e.name}  (base {base})")
    print(f"{hits} datafile(s) hold {', '.join(a.id)}", file=sys.stderr)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)
    p = sp.add_parser("info", help="header, tables, entry types")
    p.add_argument("archive")
    p.set_defaults(fn=cmd_info)
    p = sp.add_parser("list", help="list entries")
    p.add_argument("archive")
    p.add_argument("-g", "--grep", help="name contains (case-insensitive)")
    p.add_argument("-t", "--type", help="type id (hex)")
    p.add_argument("--json", action="store_true", help="one JSON object per line")
    p.set_defaults(fn=cmd_list)
    p = sp.add_parser("show", help="an entry's blocks and resources")
    p.add_argument("archive")
    p.add_argument("entry", nargs="+", help="id (0x...), exact name, or name substring")
    p.add_argument("--max", type=int, default=5, help="entries per substring match (default 5)")
    p.set_defaults(fn=cmd_show)
    p = sp.add_parser("extract", help="write entries raw (.datafile/.bin) and unpacked (resources + datafile.json)")
    p.add_argument("archive")
    p.add_argument("entry", nargs="*")
    p.add_argument("-o", "--out", default="forge_out")
    p.add_argument("--all", action="store_true", help="every entry")
    p.add_argument("--raw-only", action="store_true", help="only the raw entry bytes")
    p.add_argument("--max", type=int, default=50, help="entries per substring match (default 50)")
    p.set_defaults(fn=cmd_extract)
    p = sp.add_parser("pack", help="rebuild a datafile from an unpacked folder (datafile.json + payloads)")
    p.add_argument("dir")
    p.add_argument("-o", "--out", required=True)
    p.add_argument("--level", type=int, default=9, help="zstd level")
    p.set_defaults(fn=cmd_pack)
    p = sp.add_parser("stack", help="base + _patch_NN archives: which one provides each entry id")
    p.add_argument("base", help="the base archive, e.g. DataPC_extra.forge")
    p.add_argument("--entries", action="store_true", help="list every entry and its archive chain")
    p.add_argument("-g", "--grep", help="list entries whose name contains this")
    p.set_defaults(fn=cmd_stack)
    p = sp.add_parser("verify", help="parse every entry and check the chunk checksums")
    p.add_argument("archive")
    p.add_argument("--decompress", action="store_true", help="also decompress and check the resources")
    p.set_defaults(fn=cmd_verify)
    p = sp.add_parser("build", help="write a new archive (e.g. a patch) from datafiles or unpacked folders")
    p.add_argument("out")
    p.add_argument("datafiles", nargs="+")
    p.add_argument("--like", required=True, help="the base archive this patches (e.g. DataPC_extra.forge): "
                   "GlobalMetaFile from its newest patch, prefetch blobs from its stack")
    p.add_argument("--keep-guid", action="store_true", help="keep the copied GlobalMetaFile's archive GUID")
    p.add_argument("--level", type=int, default=9)
    p.set_defaults(fn=cmd_build)
    p = sp.add_parser("where", help="which datafiles hold a resource (by id; from the header blocks alone)")
    p.add_argument("archives", nargs="+", help="archives, or a folder (the game folder: all its archives)")
    p.add_argument("--id", action="append", required=True, help="resource id (hex); repeatable")
    p.set_defaults(fn=cmd_where)
    p = sp.add_parser("swap", help="an entry that keeps its id but holds another entry's content (unpacked folder)")
    p.add_argument("base", help="the base archive of the entry to replace, e.g. DataPC_extra_chr.forge")
    p.add_argument("entry", help="id (0x...) or exact name; taken from the newest archive of the stack")
    p.add_argument("--with", dest="src", nargs=2, required=True, metavar=("SRC_BASE", "SRC_ENTRY"),
                   help="the base archive and entry whose content goes in")
    p.add_argument("-o", "--out", required=True,
                   help="folder for the unpacked result (e.g. mods/<mod>/client/forge/<base>)")
    p.add_argument("--force", action="store_true", help="allow entries of different types")
    p.set_defaults(fn=cmd_swap)
    a = ap.parse_args(argv)
    if a.cmd == "extract" and not a.all and not a.entry:
        ap.error("extract: name entries or use --all")
    return a.fn(a) or 0


if __name__ == "__main__":
    sys.exit(main())
