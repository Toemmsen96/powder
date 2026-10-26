#!/usr/bin/env python3
"""Steep's textures (TextureMap 0xA2B7E917 + Mip 0x1D4B87A3 resources): show, export to PNG, replace.

A texture is a TextureMap resource (header + the lower mip levels) and, for big ones, one or two Mip resources
that hold the top levels and are separate entries (streamed). `info` prints a texture's header, `export` decodes
a level to PNG, `replace` encodes an image (a PNG, or a generated test pattern) to the texture's own format, size
and mip chain and writes unpacked entry folders (like forge.py extract/swap) under the same ids, ready for
`forge.py build`. `scan` checks the layout over whole archives. The format, and what is confirmed vs guessed, is
in TEXTURES.md next to this file.

Needs numpy. Pillow is optional: it reads non-PNG images, resizes, and decodes BC7 (`export` of BC7 textures);
PNGs are read and written without it. Archives are only read.
"""

from __future__ import annotations

import argparse
import glob
import hashlib
import math
import os
import struct
import sys
import zlib
from dataclasses import dataclass

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forge  # noqa: E402

try:
    import numpy as np
except ImportError:  # pragma: no cover
    sys.exit("texture.py needs numpy")

TEXTURE, MIP, MAPDESC = 0xA2B7E917, 0x1D4B87A3, 0x989DC6B2
TEX_CLASS = 0x13237FE9  # the nested object that holds the pixel data


# --- formats --------------------------------------------------------------------------------------------------

@dataclass(frozen=True)
class Fmt:
    name: str
    block: int    # bytes per 4x4 block, or 0 for an uncompressed format ...
    pixel: int    # ... with this many bytes per pixel
    note: str


FORMATS = {
    0: Fmt("BGRA8", 0, 4, "uncompressed, B G R A bytes"),
    2: Fmt("BC1", 8, 0, "DXT1, opaque: most diffuse/specular/mask maps"),
    3: Fmt("BC1A", 8, 0, "DXT1 with 1-bit alpha (UI, cut-outs)"),
    4: Fmt("BC2", 16, 0, "DXT3: explicit 4-bit alpha (UI)"),
    5: Fmt("BC3", 16, 0, "DXT5: interpolated alpha (FX, UI)"),
    9: Fmt("BC7", 16, 0, "normal maps, UI, icons"),
    11: Fmt("R8", 0, 1, "uncompressed, one byte per pixel (height maps)"),
}

USAGE = {0: "diffuse", 1: "normal", 2: "specular", 3: "height", 7: "mask1", 8: "mask2", 11: "map"}


def level_dims(w: int, h: int, d: int, level: int) -> tuple[int, int, int]:
    return max(1, w >> level), max(1, h >> level), max(1, d >> level)


def level_size(fmt: int, w: int, h: int, d: int = 1) -> int:
    f = FORMATS[fmt]
    if f.block:
        return ((w + 3) // 4) * ((h + 3) // 4) * d * f.block
    return w * h * d * f.pixel


def chain_length(w: int, h: int, d: int = 1) -> int:
    """Levels of a full chain, down to 1x1."""
    return max(w, h, d).bit_length()


# --- TextureMap / Mip payloads ----------------------------------------------------------------------------------

@dataclass
class Texture:
    """A TextureMap payload. Field names follow what the values do; see TEXTURES.md for confidence."""
    id: int
    width: int
    height: int
    depth: int
    fmt: int
    dim: int          # 1 = 2D; 2 cube, 3 volume, 0 1D (seen on engine defaults only)
    srgb: int         # 1 on diffuse/specular maps, 0 on normal/mask maps
    mips: int         # 0/1: one level stored; else the whole chain is stored (UI icons say 2..5 regardless)
    usage: int        # slot in the texture set: 0 diffuse, 1 normal, 2 specular, 7 mask1, ...
    group: int
    flags: tuple      # u8, u8 at +0x31
    unk33: int
    links: list       # [(u8 flag, u64 Mip resource id)], link i = level i
    prefix: bytes     # header bytes up to (not including) the data size field
    data: bytes

    @property
    def first_level(self) -> int:
        return len(self.links)

    def levels(self) -> list[int]:
        """The levels held inline: one if the mip count is 0 or 1, else the rest of the full chain."""
        if self.mips <= 1:
            return [self.first_level]
        return list(range(self.first_level, chain_length(self.width, self.height, self.depth)))

    def expected_size(self) -> int:
        faces = 6 if self.dim == 2 else 1  # a cube map: six faces (their order in the data is not known)
        return faces * sum(level_size(self.fmt, *level_dims(self.width, self.height, self.depth, lv))
                           for lv in self.levels())

    def payload(self, data: bytes | None = None) -> bytes:
        data = self.data if data is None else data
        return self.prefix + struct.pack("<I", len(data)) + data


def parse_texture(p: bytes) -> Texture:
    rid, typ, ver = struct.unpack_from("<QIB", p, 0)
    if typ != TEXTURE:
        raise ValueError(f"not a TextureMap (type {typ:#010x})")
    if ver != 1:
        raise ValueError(f"TextureMap version byte {ver}")
    w, h, d, fmt, dim, srgb, mips, usage, group = struct.unpack_from("<9I", p, 0x0D)
    fa, fb, unk33, nl = struct.unpack_from("<BBII", p, 0x31)
    if fa:
        raise ValueError("TextureMap without inline data (flag at +0x31 set): not supported")
    o = 0x3B
    links = []
    for _ in range(nl):
        links.append(struct.unpack_from("<BQ", p, o))
        o += 9
    if p[o:o + 9] != bytes(9):
        raise ValueError(f"unexpected bytes after the mip links at {o:#x}")
    o += 9
    inner = struct.unpack_from("<13I", p, o)
    cls, one, seven, w2, h2, d2, m2, f2, dim2, srgb2, *zeros = inner
    if cls != TEX_CLASS or (w2, h2, d2, m2, f2, dim2, srgb2) != (w, h, d, mips, fmt, dim, srgb) or any(zeros):
        raise ValueError(f"unexpected nested texture object at {o:#x}")
    o += 52
    (ds,) = struct.unpack_from("<I", p, o)
    if ds != len(p) - o - 4:
        raise ValueError(f"data size {ds} does not reach the payload's end ({len(p) - o - 4})")
    return Texture(rid, w, h, d, fmt, dim, srgb, mips, usage, group, (fa, fb), unk33, links, p[:o], p[o + 4:])


@dataclass
class Mip:
    id: int
    data: bytes
    flag: int
    texture: int
    level: int


def parse_mip(p: bytes) -> Mip:
    rid, typ, ver, size = struct.unpack_from("<QIBI", p, 0)
    if typ != MIP or ver != 1:
        raise ValueError(f"not a Mip payload (type {typ:#010x}, version {ver})")
    if 17 + size + 13 != len(p):
        raise ValueError(f"Mip data size {size} does not fit the payload ({len(p)})")
    flag, tex, level = struct.unpack_from("<BQI", p, 17 + size)
    return Mip(rid, p[17:17 + size], flag, tex, level)


def build_mip(m: Mip, data: bytes) -> bytes:
    return struct.pack("<QIBI", m.id, MIP, 1, len(data)) + data + struct.pack("<BQI", m.flag, m.texture, m.level)


# --- block decoders (numpy) -----------------------------------------------------------------------------------

def _blocks(data: bytes, w: int, h: int, size: int) -> np.ndarray:
    bw, bh = (w + 3) // 4, (h + 3) // 4
    a = np.frombuffer(data, np.uint8, bw * bh * size)
    return a.reshape(bw * bh, size)


def _unblock(px: np.ndarray, w: int, h: int) -> np.ndarray:
    """(nblocks, 16, C) in block order -> (h, w, C) image (cropped to w x h)."""
    bw, bh = (w + 3) // 4, (h + 3) // 4
    c = px.shape[-1]
    img = px.reshape(bh, bw, 4, 4, c).transpose(0, 2, 1, 3, 4).reshape(bh * 4, bw * 4, c)
    return img[:h, :w]


def _to_blocks(img: np.ndarray) -> np.ndarray:
    """(h, w, C) image -> (nblocks, 16, C), edge pixels repeated to fill partial blocks."""
    h, w, c = img.shape
    bw, bh = (w + 3) // 4, (h + 3) // 4
    if (bw * 4, bh * 4) != (w, h):
        img = np.pad(img, ((0, bh * 4 - h), (0, bw * 4 - w), (0, 0)), mode="edge")
    return img.reshape(bh, 4, bw, 4, c).transpose(0, 2, 1, 3, 4).reshape(bh * bw, 16, c)


def _expand565(c: np.ndarray) -> np.ndarray:
    r, g, b = (c >> 11) & 31, (c >> 5) & 63, c & 31
    return np.stack([(r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)], -1).astype(np.int32)


def _bc1_colors(blk: np.ndarray, four_always: bool) -> np.ndarray:
    """BC1 colour part (8 bytes per block) -> (n, 16, 4) RGBA."""
    c0 = blk[:, 0].astype(np.int32) | blk[:, 1].astype(np.int32) << 8
    c1 = blk[:, 2].astype(np.int32) | blk[:, 3].astype(np.int32) << 8
    e0, e1 = _expand565(c0), _expand565(c1)
    four = (c0 > c1) | four_always
    pal = np.zeros((len(blk), 4, 4), np.int32)
    pal[:, 0, :3], pal[:, 1, :3] = e0, e1
    pal[:, 2, :3] = np.where(four[:, None], (2 * e0 + e1) // 3, (e0 + e1) // 2)
    pal[:, 3, :3] = np.where(four[:, None], (e0 + 2 * e1) // 3, 0)
    pal[:, :3, 3] = 255
    pal[:, 3, 3] = np.where(four, 255, 0)
    idx = blk[:, 4:8].astype(np.uint32)
    bits = idx[:, 0] | idx[:, 1] << 8 | idx[:, 2] << 16 | idx[:, 3] << 24
    sel = (bits[:, None] >> (2 * np.arange(16, dtype=np.uint32))) & 3
    return np.take_along_axis(pal, sel[:, :, None].astype(np.int64).repeat(4, 2), 1)


def _bc4_values(blk: np.ndarray) -> np.ndarray:
    """BC4 (8 bytes per block, unsigned) -> (n, 16) values."""
    a0, a1 = blk[:, 0].astype(np.int32), blk[:, 1].astype(np.int32)
    pal = np.zeros((len(blk), 8), np.int32)
    pal[:, 0], pal[:, 1] = a0, a1
    eight = a0 > a1
    for i in range(1, 7):
        p8 = ((7 - i) * a0 + i * a1) // 7
        p6 = ((5 - i) * a0 + i * a1) // 5 if i < 5 else (0 if i == 5 else 255)
        pal[:, i + 1] = np.where(eight, p8, p6)
    bits = np.zeros(len(blk), np.uint64)
    for k in range(6):
        bits |= blk[:, 2 + k].astype(np.uint64) << np.uint64(8 * k)
    sel = (bits[:, None] >> (3 * np.arange(16, dtype=np.uint64))) & np.uint64(7)
    return np.take_along_axis(pal, sel.astype(np.int64), 1)


def decode_bc1(data, w, h, alpha=True):
    px = _bc1_colors(_blocks(data, w, h, 8), four_always=False)
    if not alpha:
        px[:, :, 3] = 255
    return _unblock(px, w, h)


def decode_bc2(data, w, h):
    blk = _blocks(data, w, h, 16)
    px = _bc1_colors(blk[:, 8:], four_always=True)
    a = blk[:, :8].astype(np.int32)
    nib = np.stack([a & 15, a >> 4], -1).reshape(len(blk), 16)
    px[:, :, 3] = nib * 17
    return _unblock(px, w, h)


def decode_bc3(data, w, h):
    blk = _blocks(data, w, h, 16)
    px = _bc1_colors(blk[:, 8:], four_always=True)
    px[:, :, 3] = _bc4_values(blk[:, :8])
    return _unblock(px, w, h)


def decode_bc7(data, w, h):
    try:
        from PIL import Image
    except ImportError:
        sys.exit("decoding BC7 needs Pillow (python3 -m pip install pillow)")
    bw, bh = (w + 3) // 4 * 4, (h + 3) // 4 * 4
    im = Image.frombytes("RGBA", (bw, bh), bytes(data[:level_size(9, w, h)]), "bcn", (7, "BC7"))
    return np.asarray(im, np.uint8)[:h, :w].astype(np.int32)


def decode_level(fmt: int, data: bytes, w: int, h: int) -> np.ndarray:
    """One 2D level -> (h, w, 4) RGBA int32."""
    if fmt == 0:
        return np.frombuffer(data, np.uint8, w * h * 4).reshape(h, w, 4)[:, :, [2, 1, 0, 3]].astype(np.int32)
    if fmt == 11:
        g = np.frombuffer(data, np.uint8, w * h).reshape(h, w).astype(np.int32)
        return np.stack([g, g, g, np.full_like(g, 255)], -1)
    if fmt == 2:
        return decode_bc1(data, w, h, alpha=False)
    if fmt == 3:
        return decode_bc1(data, w, h, alpha=True)
    if fmt == 4:
        return decode_bc2(data, w, h)
    if fmt == 5:
        return decode_bc3(data, w, h)
    if fmt == 9:
        return decode_bc7(data, w, h)
    raise ValueError(f"texture format {fmt} is not known")


# --- block encoders (numpy) -----------------------------------------------------------------------------------

def _principal(px: np.ndarray, iters: int = 8) -> tuple[np.ndarray, np.ndarray]:
    """Per block mean and principal axis (power iteration) of (n, 16, C) float pixels."""
    mean = px.mean(1)
    d = px - mean[:, None]
    cov = np.einsum("nki,nkj->nij", d, d)
    v = np.ones((len(px), px.shape[2])) / math.sqrt(px.shape[2])
    v = v + 0.01 * np.arange(px.shape[2])  # off the diagonal, so a pure-grey axis is not a fixed point
    for _ in range(iters):
        v = np.einsum("nij,nj->ni", cov, v)
        v /= np.maximum(np.linalg.norm(v, axis=1, keepdims=True), 1e-12)
    return mean, v


def _q565(c: np.ndarray) -> np.ndarray:
    c = np.clip(np.rint(c), 0, 255).astype(np.int32)
    return ((c[..., 0] * 31 + 127) // 255) << 11 | ((c[..., 1] * 63 + 127) // 255) << 5 | (c[..., 2] * 31 + 127) // 255


def _pack_bc1(c0, c1, sel) -> np.ndarray:
    bits = (sel.astype(np.uint32) << (2 * np.arange(16, dtype=np.uint32))).sum(1, dtype=np.uint64).astype(np.uint32)
    out = np.zeros((len(c0), 8), np.uint8)
    out[:, 0], out[:, 1] = c0 & 255, c0 >> 8
    out[:, 2], out[:, 3] = c1 & 255, c1 >> 8
    for k in range(4):
        out[:, 4 + k] = (bits >> (8 * k)) & 255
    return out


def _bc1_fit(rgb: np.ndarray, c0: np.ndarray, c1: np.ndarray, transparent=None):
    """Indices for given endpoints (4-colour mode, or 3-colour + transparent where `transparent` has pixels)."""
    e0, e1 = _expand565(c0), _expand565(c1)
    three = np.zeros(len(c0), bool) if transparent is None else transparent.any(1)
    pal = np.stack([e0, e1,
                    np.where(three[:, None], (e0 + e1) // 2, (2 * e0 + e1) // 3),
                    np.where(three[:, None], 0, (e0 + 2 * e1) // 3)], 1).astype(np.float64)
    dist = ((rgb[:, :, None, :] - pal[:, None, :, :]) ** 2).sum(-1)
    dist[three, :, 3] = np.inf  # index 3 means transparent in 3-colour mode
    sel = dist.argmin(-1)
    err = np.take_along_axis(dist, sel[..., None], -1)[..., 0]
    if transparent is not None:
        sel = np.where(transparent, 3, sel)
        err = np.where(transparent, 0, err)
    return sel, err.sum(1)


def encode_bc1(img: np.ndarray, alpha_cut: bool = False) -> bytes:
    """RGBA (h, w, 4) -> BC1. 4-colour blocks (c0 > c1); with `alpha_cut`, blocks with alpha < 128 use 3-colour
    mode and index 3 (transparent)."""
    blk = _to_blocks(img.astype(np.float64))
    rgb = blk[:, :, :3]
    transparent = (blk[:, :, 3] < 128) if alpha_cut else None
    out = []
    for lo in range(0, len(rgb), 65536):
        sl = slice(lo, lo + 65536)
        part, tr = rgb[sl], None if transparent is None else transparent[sl]
        mean, axis = _principal(part)
        t = np.einsum("nki,ni->nk", part - mean[:, None], axis)
        hi_c, lo_c = mean + axis * t.max(1)[:, None], mean + axis * t.min(1)[:, None]
        c0, c1 = _q565(hi_c), _q565(lo_c)
        three = np.zeros(len(part), bool) if tr is None else tr.any(1)
        # 4-colour mode needs c0 > c1; 3-colour mode needs c0 <= c1
        swap = np.where(three, c0 > c1, c0 < c1)
        c0, c1 = np.where(swap, c1, c0), np.where(swap, c0, c1)
        sel, err = _bc1_fit(part, c0, c1, tr)
        # one least-squares refinement of the endpoints from the chosen indices (kept where it helps)
        w = np.array([[1, 0], [0, 1], [2 / 3, 1 / 3], [1 / 3, 2 / 3]])[np.minimum(sel, 3)]
        if tr is not None:
            w = np.where(three[:, None, None], np.array([[1, 0], [0, 1], [.5, .5], [0, 0]])[sel], w)
            w = np.where(tr[:, :, None], 0, w)
        ata = np.einsum("nki,nkj->nij", w, w) + np.eye(2) * 1e-6
        atb = np.einsum("nki,nkc->nic", w, part)
        ends = np.linalg.solve(ata, atb)
        r0, r1 = _q565(ends[:, 0]), _q565(ends[:, 1])
        swap = np.where(three, r0 > r1, r0 < r1)
        r0, r1 = np.where(swap, r1, r0), np.where(swap, r0, r1)
        rsel, rerr = _bc1_fit(part, r0, r1, tr)
        ok = (rerr < err) & ((r0 != r1) | three)
        c0, c1 = np.where(ok, r0, c0), np.where(ok, r1, c1)
        sel = np.where(ok[:, None], rsel, sel)
        same = (c0 == c1) & ~three
        sel = np.where(same[:, None], 0, sel)  # c0 == c1: a flat block, index 0
        out.append(_pack_bc1(c0, c1, sel))
    return np.concatenate(out).tobytes()


def _encode_bc4(vals: np.ndarray) -> np.ndarray:
    """(n, 16) values 0..255 -> (n, 8) BC4 blocks (8-value mode: a0 > a1, or a flat block)."""
    v = vals.astype(np.int32)
    a0, a1 = v.max(1), v.min(1)
    pal = np.stack([a0, a1] + [((7 - i) * a0 + i * a1) // 7 for i in range(1, 7)], 1)
    sel = np.abs(v[:, :, None] - pal[:, None, :]).argmin(-1)
    sel = np.where((a0 == a1)[:, None], 0, sel)
    bits = (sel.astype(np.uint64) << (3 * np.arange(16, dtype=np.uint64))).sum(1, dtype=np.uint64)
    out = np.zeros((len(v), 8), np.uint8)
    out[:, 0], out[:, 1] = a0, a1
    for k in range(6):
        out[:, 2 + k] = (bits >> np.uint64(8 * k)) & np.uint64(255)
    return out


def _encode_bc1_part(img: np.ndarray) -> np.ndarray:
    return np.frombuffer(encode_bc1(img), np.uint8).reshape(-1, 8)


def encode_bc2(img: np.ndarray) -> bytes:
    blk = _to_blocks(img)
    a = (np.clip(blk[:, :, 3], 0, 255).astype(np.int32) * 15 + 127) // 255
    alpha = (a[:, 0::2] | a[:, 1::2] << 4).astype(np.uint8)
    return np.concatenate([alpha, _encode_bc1_part(img)], 1).tobytes()


def encode_bc3(img: np.ndarray) -> bytes:
    blk = _to_blocks(img)
    return np.concatenate([_encode_bc4(np.clip(blk[:, :, 3], 0, 255)), _encode_bc1_part(img)], 1).tobytes()


BC7_W4 = np.array([0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64])


def encode_bc7(img: np.ndarray) -> bytes:
    """RGBA -> BC7, every block in mode 6 (one subset, RGBA 7.7.7.7 endpoints + p-bit, 4-bit indices)."""
    blk = _to_blocks(img.astype(np.float64))
    out = []
    for lo in range(0, len(blk), 65536):
        px = blk[lo:lo + 65536]
        n = len(px)
        mean, axis = _principal(px)
        t = np.einsum("nki,ni->nk", px - mean[:, None], axis)
        ends = np.stack([mean + axis * t.min(1)[:, None], mean + axis * t.max(1)[:, None]], 1)  # (n, 2, 4)
        ends = np.clip(ends, 0, 255)
        best_err, best = None, None
        for p0 in (0, 1):
            for p1 in (0, 1):
                pb = np.array([p0, p1])[None, :, None]
                c7 = np.clip(np.rint((ends - pb) / 2), 0, 127).astype(np.int32)
                e = (c7 << 1) | pb
                pal = ((64 - BC7_W4)[None, :, None] * e[:, 0:1] + BC7_W4[None, :, None] * e[:, 1:2] + 32) >> 6
                dist = ((px[:, :, None, :] - pal[:, None, :, :]) ** 2).sum(-1)
                sel = dist.argmin(-1)
                err = np.take_along_axis(dist, sel[..., None], -1)[..., 0].sum(1)
                if best_err is None:
                    best_err, best = err, (c7, np.broadcast_to(np.array([p0, p1]), (n, 2)).copy(), sel)
                else:
                    better = err < best_err
                    best_err = np.where(better, err, best_err)
                    cand = (c7, np.broadcast_to(np.array([p0, p1]), (n, 2)), sel)
                    best = tuple(np.where(better.reshape((n,) + (1,) * (b.ndim - 1)), c, b)
                                 for b, c in zip(best, cand, strict=True))
        c7, pbits, sel = best
        flip = sel[:, 0] >= 8  # the anchor (pixel 0) index has an implicit 0 top bit: swap the endpoints
        c7 = np.where(flip[:, None, None], c7[:, ::-1], c7)
        pbits = np.where(flip[:, None], pbits[:, ::-1], pbits)
        sel = np.where(flip[:, None], 15 - sel, sel)
        fields = [(np.full(n, 1 << 6), 7)]  # mode 6: six 0 bits, then a 1
        fields += [(c7[:, k, ch], 7) for ch in range(4) for k in (0, 1)]
        fields += [(pbits[:, 0], 1), (pbits[:, 1], 1), (sel[:, 0], 3)] + [(sel[:, i], 4) for i in range(1, 16)]
        bits = np.zeros((n, 128), np.uint8)
        pos = 0
        for vals, width in fields:
            for b in range(width):
                bits[:, pos + b] = (vals >> b) & 1
            pos += width
        out.append(np.packbits(bits, axis=1, bitorder="little"))
    return np.concatenate(out).tobytes()


def encode_level(fmt: int, img: np.ndarray) -> bytes:
    img = np.clip(np.rint(img), 0, 255)
    if fmt == 0:
        return img[:, :, [2, 1, 0, 3]].astype(np.uint8).tobytes()
    if fmt == 11:
        return img[:, :, 0].astype(np.uint8).tobytes()
    if fmt == 2:
        return encode_bc1(img)
    if fmt == 3:
        return encode_bc1(img, alpha_cut=True)
    if fmt == 4:
        return encode_bc2(img)
    if fmt == 5:
        return encode_bc3(img)
    if fmt == 9:
        return encode_bc7(img)
    raise ValueError(f"texture format {fmt} is not known")


# --- images: PNG in/out without Pillow, resize, mips, patterns ---------------------------------------------------

def write_png(path: str, img: np.ndarray):
    img = np.clip(img, 0, 255).astype(np.uint8)
    h, w, c = img.shape
    ctype = {1: 0, 2: 4, 3: 2, 4: 6}[c]
    raw = np.concatenate([np.zeros((h, 1), np.uint8), img.reshape(h, w * c)], 1).tobytes()

    def chunk(tag, body):
        return struct.pack(">I", len(body)) + tag + body + struct.pack(">I", zlib.crc32(tag + body))
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, ctype, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


def read_png(path: str) -> np.ndarray:
    """8-bit, non-interlaced PNG (grey, grey+alpha, RGB, RGBA, palette) -> (h, w, 4) RGBA."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")
    pos, idat, plte, trns = 8, b"", None, None
    while pos < len(data):
        (n,) = struct.unpack_from(">I", data, pos)
        tag, body = data[pos + 4:pos + 8], data[pos + 8:pos + 8 + n]
        pos += 12 + n
        if tag == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
        elif tag == b"PLTE":
            plte = np.frombuffer(body, np.uint8).reshape(-1, 3)
        elif tag == b"tRNS":
            trns = np.frombuffer(body, np.uint8)
        elif tag == b"IDAT":
            idat += body
        elif tag == b"IEND":
            break
    if depth != 8 or interlace:
        raise ValueError(f"{path}: only 8-bit non-interlaced PNGs are read without Pillow")
    c = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    raw = np.frombuffer(zlib.decompress(idat), np.uint8).reshape(h, 1 + w * c)
    out = np.zeros((h, w * c), np.int32)
    prev = np.zeros(w * c, np.int32)
    for y in range(h):
        ft, line = raw[y, 0], raw[y, 1:].astype(np.int32)
        if ft == 0:
            cur = line
        elif ft == 2:
            cur = (line + prev) & 255
        else:
            cur = np.zeros(w * c, np.int32)
            for x in range(w * c):
                a = cur[x - c] if x >= c else 0
                b = prev[x]
                if ft == 1:
                    cur[x] = (line[x] + a) & 255
                elif ft == 3:
                    cur[x] = (line[x] + (a + b) // 2) & 255
                else:
                    cc = prev[x - c] if x >= c else 0
                    p = a + b - cc
                    pa, pb, pc = abs(p - a), abs(p - b), abs(p - cc)
                    pr = a if pa <= pb and pa <= pc else (b if pb <= pc else cc)
                    cur[x] = (line[x] + pr) & 255
        out[y] = cur
        prev = cur
    img = out.reshape(h, w, c)
    if ctype == 3:
        rgb = plte[img[:, :, 0]].astype(np.int32)
        a = np.full((h, w, 1), 255, np.int32)
        if trns is not None:
            ta = np.full(256, 255, np.int32)
            ta[:len(trns)] = trns
            a = ta[img[:, :, 0]][:, :, None]
        return np.concatenate([rgb, a], 2)
    if c == 1:
        return np.concatenate([img.repeat(3, 2), np.full((h, w, 1), 255, np.int32)], 2)
    if c == 2:
        return np.concatenate([img[:, :, :1].repeat(3, 2), img[:, :, 1:]], 2)
    if c == 3:
        return np.concatenate([img, np.full((h, w, 1), 255, np.int32)], 2)
    return img


def read_image(path: str) -> np.ndarray:
    try:
        from PIL import Image
        with Image.open(path) as im:
            return np.asarray(im.convert("RGBA"), np.uint8).astype(np.int32)
    except ImportError:
        return read_png(path)


def resize(img: np.ndarray, w: int, h: int) -> np.ndarray:
    if img.shape[1] == w and img.shape[0] == h:
        return img
    try:
        from PIL import Image
        im = Image.fromarray(np.clip(img, 0, 255).astype(np.uint8), "RGBA")
        return np.asarray(im.resize((w, h), Image.LANCZOS), np.uint8).astype(np.int32)
    except ImportError:  # nearest neighbour
        ys = (np.arange(h) * img.shape[0]) // h
        xs = (np.arange(w) * img.shape[1]) // w
        return img[ys][:, xs]


def downsample(img: np.ndarray) -> np.ndarray:
    """2x2 box filter (an odd or 1-pixel side is padded by repeating its edge)."""
    h, w, c = img.shape
    nh, nw = max(1, h // 2), max(1, w // 2)
    f = img.astype(np.float64)
    if h > 1 and w > 1:
        return f[:nh * 2, :nw * 2].reshape(nh, 2, nw, 2, c).mean((1, 3))
    if h > 1:
        return f[:nh * 2].reshape(nh, 2, w, c).mean(1)
    if w > 1:
        return f[:, :nw * 2].reshape(h, nw, 2, c).mean(2)
    return f


def mip_chain(img: np.ndarray, n: int) -> list[np.ndarray]:
    out = [img.astype(np.float64)]
    for _ in range(n - 1):
        out.append(downsample(out[-1]))
    return out


# 5x7 font, rows top to bottom, 5 bits each (bit 4 = left column)
FONT = {
    "A": (14, 17, 17, 31, 17, 17, 17), "B": (30, 17, 17, 30, 17, 17, 30), "C": (14, 17, 16, 16, 16, 17, 14),
    "D": (30, 17, 17, 17, 17, 17, 30), "E": (31, 16, 16, 30, 16, 16, 31), "F": (31, 16, 16, 30, 16, 16, 16),
    "G": (14, 17, 16, 23, 17, 17, 15), "H": (17, 17, 17, 31, 17, 17, 17), "I": (14, 4, 4, 4, 4, 4, 14),
    "J": (7, 2, 2, 2, 2, 18, 12), "K": (17, 18, 20, 24, 20, 18, 17), "L": (16, 16, 16, 16, 16, 16, 31),
    "M": (17, 27, 21, 21, 17, 17, 17), "N": (17, 17, 25, 21, 19, 17, 17), "O": (14, 17, 17, 17, 17, 17, 14),
    "P": (30, 17, 17, 30, 16, 16, 16), "Q": (14, 17, 17, 17, 21, 18, 13), "R": (30, 17, 17, 30, 20, 18, 17),
    "S": (15, 16, 16, 14, 1, 1, 30), "T": (31, 4, 4, 4, 4, 4, 4), "U": (17, 17, 17, 17, 17, 17, 14),
    "V": (17, 17, 17, 17, 17, 10, 4), "W": (17, 17, 17, 21, 21, 21, 10), "X": (17, 17, 10, 4, 10, 17, 17),
    "Y": (17, 17, 10, 4, 4, 4, 4), "Z": (31, 1, 2, 4, 8, 16, 31), "0": (14, 17, 19, 21, 25, 17, 14),
    "1": (4, 12, 4, 4, 4, 4, 14), "2": (14, 17, 1, 2, 4, 8, 31), "3": (31, 2, 4, 2, 1, 17, 14),
    "4": (2, 6, 10, 18, 31, 2, 2), "5": (31, 16, 30, 1, 1, 17, 14), "6": (6, 8, 16, 30, 17, 17, 14),
    "7": (31, 1, 2, 4, 8, 8, 8), "8": (14, 17, 17, 14, 17, 17, 14), "9": (14, 17, 17, 15, 1, 2, 12),
    " ": (0,) * 7, "-": (0, 0, 0, 31, 0, 0, 0), "!": (4, 4, 4, 4, 4, 0, 4), ".": (0, 0, 0, 0, 0, 12, 12),
}


def text_mask(text: str, scale: int) -> np.ndarray:
    glyphs = [FONT.get(ch, FONT[" "]) for ch in text.upper()]
    m = np.zeros((7, 6 * len(glyphs) - 1), bool)
    for k, rows in enumerate(glyphs):
        for y, bits in enumerate(rows):
            for x in range(5):
                m[y, 6 * k + x] = bool(bits >> (4 - x) & 1)
    return m.repeat(scale, 0).repeat(scale, 1)


def pattern(name: str, w: int, h: int, text: str = "POWDER") -> np.ndarray:
    """A loud test image: `checker` (magenta/yellow cells with the text in each) or `stripes` (diagonal bands
    with the text along them). Opaque RGBA."""
    yy, xx = np.mgrid[0:h, 0:w]
    img = np.zeros((h, w, 4), np.int32)
    img[:, :, 3] = 255
    magenta, yellow, cyan, black, white = (255, 0, 200), (255, 230, 0), (0, 230, 255), (0, 0, 0), (255, 255, 255)
    if name == "checker":
        cell = max(4, min(w, h) // 8)
        odd = ((xx // cell) + (yy // cell)) % 2 == 1
        img[:, :, :3] = np.where(odd[:, :, None], magenta, yellow)
        edge = (xx % cell < max(1, cell // 32)) | (yy % cell < max(1, cell // 32))
        img[edge, :3] = cyan
        if text:
            scale = max(1, (cell * 9 // 10) // (6 * len(text)))
            m = text_mask(text, scale)
            mh, mw = m.shape
            if mh < cell and mw < cell:
                for cy in range(0, h - cell + 1, cell):
                    for cx in range(0, w - cell + 1, cell):
                        oy, ox = cy + (cell - mh) // 2, cx + (cell - mw) // 2
                        colour = white if ((cx // cell) + (cy // cell)) % 2 else black
                        img[oy:oy + mh, ox:ox + mw, :3][m] = colour
    elif name == "stripes":
        band = max(4, min(w, h) // 12)
        k = ((xx + yy) // band) % 3
        img[:, :, :3] = np.choose(k[:, :, None], [np.array(magenta), np.array(yellow), np.array(cyan)])
        if text:
            scale = max(1, band // 10)
            m = text_mask(text, scale)
            mh, mw = m.shape
            for oy in range(band // 4, h - mh, max(mh * 3, band)):
                for ox in range((oy // 2) % max(1, mw + 8 * scale), w - mw, mw + 8 * scale):
                    img[oy:oy + mh, ox:ox + mw, :3][m] = black
    else:
        raise ValueError(f"unknown pattern {name!r} (checker, stripes)")
    return img


# --- finding textures and their mips ---------------------------------------------------------------------------

def open_forge(path: str, cache: dict) -> forge.Forge:
    if path not in cache:
        cache[path] = forge.Forge(path)
    return cache[path]


def find_entry(archive: str, spec: str, cache: dict, newest: bool) -> tuple[forge.Forge, forge.Entry]:
    """`spec` in `archive` itself, or (newest=True) in the newest archive of its stack that has it."""
    paths = [p for p in forge.patch_stack(archive) if os.path.exists(p)] if newest else [archive]
    for p in reversed(paths):
        fg = open_forge(p, cache)
        found = fg.find(spec)
        if len(found) > 1:
            sys.exit(f"{spec}: {len(found)} entries in {os.path.basename(p)} match; give the id or the exact name")
        if found:
            return fg, found[0]
    sys.exit(f"{spec}: not in {os.path.basename(archive)}" + ("'s stack" if newest else ""))


def find_mip_entry(rid: int, archive: str, cache: dict, anywhere: bool) -> tuple[forge.Forge, forge.Entry] | None:
    """A Mip resource's own entry: the newest copy in `archive`'s stack, else (anywhere) in any archive of the
    same folder (big textures' Mips can live in another archive than a copy of their TextureMap)."""
    stack = [p for p in forge.patch_stack(archive) if os.path.exists(p)]
    for p in reversed(stack):
        fg = open_forge(p, cache)
        if rid in fg.by_id:
            return fg, fg.by_id[rid]
    if anywhere:
        for p in sorted(glob.glob(os.path.join(os.path.dirname(os.path.abspath(archive)), "*.forge"))):
            if p in stack:
                continue
            fg = open_forge(p, cache)
            if rid in fg.by_id:
                return fg, fg.by_id[rid]
    return None


def texture_resources(df: forge.Datafile, tex_id: int | None) -> list[forge.Resource]:
    res = [r for r in df.resources if r.type == TEXTURE and (tex_id is None or r.id == tex_id)]
    if not res:
        raise SystemExit("no TextureMap resource" + (f" {tex_id:#x}" if tex_id else "") + " in that entry")
    return res


def describe(t: Texture, name: str, out=print):
    f = FORMATS.get(t.fmt)
    out(f"{name}  id={t.id:#x}  {t.width}x{t.height}" + (f"x{t.depth}" if t.depth > 1 else "")
        + f"  format {t.fmt} ({f.name if f else '?'})  {'sRGB' if t.srgb else 'linear'}?  "
        f"usage {t.usage} ({USAGE.get(t.usage, '?')})  dim {t.dim}  mips {t.mips} (chain "
        f"{chain_length(t.width, t.height, t.depth)})  group {t.group}  +0x33={t.unk33}  flags={t.flags}")
    for lv, (fl, mid) in enumerate(t.links):
        w, h, _ = level_dims(t.width, t.height, t.depth, lv)
        out(f"  level {lv}: {w}x{h} in Mip {mid:#x}" + (f" (link flag {fl})" if fl else ""))
    lv = t.levels()
    if lv:
        w, h, _ = level_dims(t.width, t.height, t.depth, lv[0])
        out(f"  levels {lv[0]}..{lv[-1]} ({w}x{h} .. 1x1) inline: {len(t.data)} bytes"
            + ("" if f is None or t.dim != 1 or t.expected_size() == len(t.data)
               else f", expected {t.expected_size()} (!)"))


# --- commands -------------------------------------------------------------------------------------------------

def load_datafile(a, cache) -> tuple[forge.Datafile, str, str | None]:
    """(datafile, label, archive) from ARCHIVE ENTRY, a .datafile, or an unpacked folder."""
    if os.path.isdir(a.source):
        m, res = forge.load_unpacked(a.source)
        return forge.Datafile(None, None, res), m["name"], None
    if a.source.endswith(".datafile"):
        with open(a.source, "rb") as f:
            return forge.parse_datafile(f.read()), os.path.basename(a.source), None
    if not a.entry:
        sys.exit("name the entry (id or name) in the archive")
    fg, e = find_entry(a.source, a.entry, cache, newest=a.newest)
    return forge.parse_datafile(fg.read(e)), f"{e.name} ({os.path.basename(fg.path)})", fg.path


def cmd_info(a):
    cache: dict = {}
    df, label, archive = load_datafile(a, cache)
    print(label)
    for r in df.resources:
        if r.type == TEXTURE:
            describe(parse_texture(r.payload), r.name)
            if archive:
                for _, mid in parse_texture(r.payload).links:
                    hit = find_mip_entry(mid, archive, cache, anywhere=True)
                    print(f"    Mip {mid:#x}: " + (f"entry {hit[1].name} in {os.path.basename(hit[0].path)}"
                                                   if hit else "in no archive of this folder"))
        elif r.type == MIP:
            m = parse_mip(r.payload)
            print(f"{r.name}  Mip id={m.id:#x}  level {m.level} of texture {m.texture:#x}  {len(m.data)} bytes")


def texture_levels(a, cache) -> tuple[Texture, str, dict[int, bytes]]:
    """The texture and its levels' data {level: bytes}, Mip levels included where they can be found."""
    df, label, archive = load_datafile(a, cache)
    t = parse_texture(texture_resources(df, int(a.texture, 16) if a.texture else None)[0].payload)
    data, pos = {}, 0
    for lv in t.levels():
        n = level_size(t.fmt, *level_dims(t.width, t.height, t.depth, lv))
        data[lv] = t.data[pos:pos + n]
        pos += n
    for _, mid in t.links:
        hit = find_mip_entry(mid, archive, cache, anywhere=True) if archive else None
        if hit:
            for r in forge.parse_datafile(hit[0].read(hit[1])).resources:
                if r.id == mid:
                    m = parse_mip(r.payload)
                    data[m.level] = m.data
    return t, label, data


def cmd_export(a):
    cache: dict = {}
    t, label, data = texture_levels(a, cache)
    if t.fmt not in FORMATS or t.dim != 1:
        sys.exit(f"export: format {t.fmt} / dim {t.dim} is not supported")
    levels = sorted(data) if a.all_levels else [a.level if a.level is not None else min(data)]
    for lv in levels:
        if lv not in data:
            sys.exit(f"level {lv} is not available (have {sorted(data)})")
        w, h, _ = level_dims(t.width, t.height, t.depth, lv)
        img = decode_level(t.fmt, data[lv], w, h)
        if not a.raw_rows:
            img = img[::-1]  # rows are stored bottom-up
        out = a.out if len(levels) == 1 else f"{os.path.splitext(a.out)[0]}_L{lv}.png"
        write_png(out, img if (img[:, :, 3] != 255).any() else img[:, :, :3])
        print(f"{out}: level {lv}, {w}x{h}, {FORMATS[t.fmt].name}")


def source_image(a, w: int, h: int) -> np.ndarray:
    if a.image:
        return resize(read_image(a.image), w, h)
    return pattern(a.pattern, w, h, a.text)


def cmd_replace(a):
    cache: dict = {}
    fg, e = find_entry(a.base, a.entry, cache, newest=True)
    df = forge.parse_datafile(fg.read(e))
    tex_res = texture_resources(df, int(a.texture, 16) if a.texture else None)
    if len(tex_res) > 1:
        sys.exit(f"{e.name} holds {len(tex_res)} TextureMaps: pick one with --texture")
    t = parse_texture(tex_res[0].payload)
    if t.fmt not in FORMATS or t.dim != 1 or t.depth != 1:
        sys.exit(f"replace: format {t.fmt} / dim {t.dim} / depth {t.depth} is not supported")
    img = source_image(a, t.width, t.height)
    if not a.raw_rows:
        img = img[::-1]  # rows are stored bottom-up: an upright image goes in upside down
    chain = mip_chain(img, chain_length(t.width, t.height))
    enc = {lv: encode_level(t.fmt, chain[lv]) for lv in range(len(chain))}
    for lv, blob in enc.items():
        w, h, _ = level_dims(t.width, t.height, 1, lv)
        assert len(blob) == level_size(t.fmt, w, h), (lv, len(blob))
    if a.preview:
        prev = decode_level(t.fmt, enc[0], t.width, t.height)
        write_png(a.preview, prev if a.raw_rows else prev[::-1])
    src = os.path.basename(a.image) if a.image else f"pattern {a.pattern} {a.text!r}"
    sha = hashlib.sha256(b"".join(enc[lv] for lv in sorted(enc))).hexdigest()[:16]
    note = {"texture": {"id": f"{t.id:#x}", "format": f"{t.fmt} {FORMATS[t.fmt].name}",
                        "size": f"{t.width}x{t.height}", "levels": len(chain), "source": src, "sha256": sha}}
    # the entry itself: every resource unchanged except this TextureMap (same header, new inline levels)
    new_tex = t.payload(b"".join(enc[lv] for lv in t.levels()))
    assert parse_texture(new_tex).expected_size() == len(parse_texture(new_tex).data)
    res = [forge.Resource(r.id, r.type, r.name, new_tex if r.id == t.id and r.type == TEXTURE else r.payload,
                          r.flag, r.pre, r.extra) for r in df.resources]
    write_entry(a.out, e, res, note)
    # the Mip entries that hold the top levels, where this stack has them
    for lv, (_, mid) in enumerate(t.links):
        hit = find_mip_entry(mid, a.base, cache, anywhere=False)
        if not hit:
            other = find_mip_entry(mid, a.base, cache, anywhere=True)
            print(f"  level {lv} lives in Mip {mid:#x}, which is not in this stack"
                  + (f" ({other[1].name} in {os.path.basename(other[0].path)}: replace it there, with that base)"
                     if other else " (nor in this folder)"))
            continue
        mfg, me = hit
        mdf = forge.parse_datafile(mfg.read(me))
        res = []
        for r in mdf.resources:
            if r.type == MIP and r.id == mid:
                m = parse_mip(r.payload)
                if m.texture != t.id or m.level != lv or len(m.data) != len(enc[lv]):
                    sys.exit(f"Mip {mid:#x}: level {m.level} of {m.texture:#x}, {len(m.data)} bytes: not level {lv} "
                             f"of {t.id:#x} ({len(enc[lv])} bytes)")
                res.append(forge.Resource(r.id, r.type, r.name, build_mip(m, enc[lv]), r.flag, r.pre, r.extra))
            else:
                res.append(r)
        write_entry(a.out, me, res, note, f"  level {lv} ({os.path.basename(mfg.path)})")


def write_entry(out: str, e: forge.Entry, res: list, note: dict, label: str = ""):
    data = forge.build_datafile(res)
    check = forge.parse_datafile(data)
    assert check.resources[0].id == e.id and [r.payload for r in check.resources] == [r.payload for r in res]
    base = os.path.join(out, f"{e.id:016x}_{forge.safe_name(e.name)}")
    forge.write_unpacked(base, e.id, e.name, e.type, res, **note)
    print(f"{base}/: {e.name}{label}, {len(res)} resource(s)")


def cmd_scan(a):
    """Every TextureMap and Mip in the archives: does the layout parse, do the sizes add up?"""
    from collections import Counter
    fmts, problems, mips = Counter(), Counter(), Counter()
    examples = {}
    links: dict[int, tuple] = {}   # Mip id -> (texture id, level, expected bytes)
    seen: dict[int, tuple] = {}    # Mip id -> (texture id, level, bytes, where)
    for path in a.archives:
        fg = forge.Forge(path)
        for e in fg.entries:
            if e.type not in (TEXTURE, MIP, MAPDESC) or (a.limit and fmts.total() + mips.total() > a.limit):
                continue
            for r in forge.parse_datafile(fg.read(e)).resources:
                try:
                    if r.type == TEXTURE:
                        t = parse_texture(r.payload)
                        key = (t.fmt, FORMATS[t.fmt].name if t.fmt in FORMATS else "?", t.dim)
                        fmts[key] += 1
                        if t.fmt not in FORMATS:  # bytes per pixel of the inline levels, as if a full chain
                            px = sum(math.prod(level_dims(t.width, t.height, t.depth, lv)) for lv in t.levels())
                            examples.setdefault(f"fmt {t.fmt}", f"{os.path.basename(path)} {r.name} {t.width}x"
                                                f"{t.height} dim {t.dim} mips {t.mips}: {len(t.data) / px:.3f} B/px")
                        if t.fmt in FORMATS and t.expected_size() != len(t.data):
                            raise ValueError(f"data {len(t.data)} != chain {t.expected_size()}")
                        for lv, (_, mid) in enumerate(t.links):
                            size = (level_size(t.fmt, *level_dims(t.width, t.height, t.depth, lv))
                                    if t.fmt in FORMATS and t.dim == 1 else None)
                            links[mid] = (t.id, lv, size)
                    elif r.type == MIP:
                        m = parse_mip(r.payload)
                        mips[m.level] += 1
                        seen[m.id] = (m.texture, m.level, len(m.data), f"{os.path.basename(path)} {r.name}")
                except Exception as ex:  # report and go on
                    k = str(ex).split(" at ")[0][:60]
                    problems[k] += 1
                    examples.setdefault(k, f"{os.path.basename(path)} {r.name}")
        print(f"{os.path.basename(path)}: done", file=sys.stderr)
    print("TextureMaps by (format, name, dim):")
    for k, n in sorted(fmts.items()):
        print(f"  {k}: {n}")
    print(f"Mips by level: {dict(sorted(mips.items()))}")
    ok = 0
    for mid, (tex, lv, n, where) in seen.items():
        want = links.get(mid)
        if want is None:
            problems["Mip not linked from any TextureMap scanned"] += 1
            examples.setdefault("Mip not linked from any TextureMap scanned", where)
        elif want[:2] != (tex, lv) or (want[2] is not None and want[2] != n):
            problems["Mip disagrees with its TextureMap link"] += 1
            examples.setdefault("Mip disagrees with its TextureMap link", f"{where} {want} vs {(tex, lv, n)}")
        else:
            ok += 1
    print(f"Mips matching their TextureMap's link (texture id, level, level size): {ok} of {len(seen)}; "
          f"links to Mips not scanned: {len(set(links) - set(seen))}")
    for k, v in examples.items():
        if k.startswith("fmt "):
            print(f"  unknown {k}: e.g. {v}")
    print(f"problems: {problems.total()}")
    for k, n in problems.most_common():
        print(f"  {n:6}  {k}   e.g. {examples[k]}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)

    def src(p):
        p.add_argument("source", help="an archive (then name the entry), a .datafile, or an unpacked entry folder")
        p.add_argument("entry", nargs="?", help="entry id (0x...) or exact name")
        p.add_argument("--newest", action="store_true", help="take the entry from the newest archive of the stack")
        p.add_argument("--texture", help="the TextureMap resource id, if the entry holds several")

    p = sp.add_parser("info", help="a texture's header: format, size, mips, where its Mip levels live")
    src(p)
    p.set_defaults(fn=cmd_info)
    p = sp.add_parser("export", help="decode a level to PNG (top level by default, Mip entries included)")
    src(p)
    p.add_argument("-o", "--out", required=True)
    p.add_argument("--level", type=int, help="mip level (0 = full size)")
    p.add_argument("--all-levels", action="store_true", help="every available level, as OUT_L<n>.png")
    p.add_argument("--raw-rows", action="store_true", help="rows in storage order (bottom-up), not flipped upright")
    p.set_defaults(fn=cmd_export)
    p = sp.add_parser("replace", help="encode an image as this texture (same format, size, mips) -> unpacked entries")
    p.add_argument("base", help="the base archive, e.g. DataPC_extra_chr.forge (the newest copy of the entry is used)")
    p.add_argument("entry", help="the texture's entry (id or exact name), or an entry that embeds it")
    p.add_argument("--texture", help="the TextureMap resource id, if the entry holds several")
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--image", help="an image file (PNG without Pillow); resized to the texture's size")
    g.add_argument("--pattern", choices=("checker", "stripes"), help="a generated test pattern")
    p.add_argument("--text", default="POWDER", help="text written into the pattern (default POWDER)")
    p.add_argument("--preview", help="also write the encoded top level, decoded again, to this PNG")
    p.add_argument("--raw-rows", action="store_true", help="the image is in storage order (bottom-up): no flip")
    p.add_argument("-o", "--out", required=True,
                   help="folder for the unpacked entries (e.g. mods/<mod>/client/forge/<base>)")
    p.set_defaults(fn=cmd_replace)
    p = sp.add_parser("scan", help="parse every TextureMap/Mip of the archives and check the sizes")
    p.add_argument("archives", nargs="+")
    p.add_argument("--limit", type=int, default=0, help="stop after about this many resources")
    p.set_defaults(fn=cmd_scan)
    a = ap.parse_args(argv)
    return a.fn(a) or 0


if __name__ == "__main__":
    sys.exit(main())
