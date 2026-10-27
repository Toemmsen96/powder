# Steep save files (savegames\<profile id>\3279\N.save): read, dump, and move between accounts.
#
# Layout (2026-10-26, from the user's saves; see README.md):
#   0x000 u64 0x224 (header size)   0x008 32 ASCII hex: a save-instance id (new each game session)
#   0x028 UTF-16 name "STEEP[n]-tm_YYYY_MM_DD_hh_mm_ss" (n = 1/4 and their mirrors 51/54), zero-padded to 0x224
#   0x224 body, XOR'd with a 16-byte key that is per ACCOUNT (the binding that made copying a save between
#         accounts fail), aligned to the END of the body: byte i uses key[(i + A - len(body)) % 16], A per account.
#   The plain body is a list of sections: a 24-byte header (u32 ?, u32 ?, u32 section id - hex-speak like
#   0xcafe00, 0xc0ffeeee, 0x11face11 (gear) -, u32, u32, u32 unpacked size) followed by an AnvilNext compressed
#   block (magic 0x1004FA9957FBAA33, version 2, codec 0 = LZO1X, one stream chunk with an Adler-32), the same
#   block format as the .forge archives (tools/forge/forge.py decompresses it).
#
# An account's key is recovered from any one of its saves: the most common byte per position (the plain body has
# many zero bytes), aligned by finding the block magic at plain offset 0x18.
#
#   python3 savegame.py info <save>...                       sections of a save (key from the save itself)
#   python3 savegame.py rekey <src.save> <sample of the target account>.save <out.save>
#       re-encode src for the account that wrote the sample (same plain body, same header)
import collections, os, struct, sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../forge"))
import forge  # noqa: E402

HDR = 0x224
MAGIC = struct.pack("<Q", forge.DATAFILE_MAGIC)


def account_key(save: bytes) -> tuple[bytes, int]:
    """(key, A) of the account that wrote this save, such that body[i] ^ key[(i + A - len) % 16] is plain."""
    body = save[HDR:]
    freq = bytes(collections.Counter(body[i::16]).most_common(1)[0][0] for i in range(16))
    for a in range(16):
        plain = xor(body[:64], freq, a, len(body))
        if plain[0x18:0x20] == MAGIC:
            return freq, a
    raise ValueError("no block magic at plain offset 0x18: not a save, or a key this tool can't recover")


def xor(body: bytes, key: bytes, a: int, total: int) -> bytes:
    return bytes(c ^ key[(i + a - total) % 16] for i, c in enumerate(body))


def plain_body(save: bytes) -> bytes:
    key, a = account_key(save)
    body = save[HDR:]
    return xor(body, key, a, len(body))


def sections(plain: bytes):
    """(offset, section id, block) for each compressed block in the plain body."""
    pos, out = 0, []
    while True:
        i = plain.find(MAGIC, pos)
        if i < 0:
            return out
        sid = struct.unpack_from("<I", plain, i - 16)[0] if i >= 16 else None
        magic, ver, codec, mc = struct.unpack_from("<QHBI", plain, i)
        one, cs, us, ck = struct.unpack_from("<BIII", plain, i + 15)
        out.append((i, sid, codec, cs, us, ck, plain[i + 28:i + 28 + cs]))
        pos = i + 8


def cmd_info(paths):
    for p in paths:
        save = open(p, "rb").read()
        name = save[0x28:HDR].decode("utf-16le", "replace").split("\0")[0]
        key, a = account_key(save)
        plain = plain_body(save)
        print(f"{p}: {name}, instance {save[8:40].decode()}, body {len(save) - HDR} B, key {key.hex()} A={a}")
        for off, sid, codec, cs, us, ck, data in sections(plain):
            ok = forge.adler32_0(data) == ck
            try:
                size = len(forge.decompress(codec, data, us)) if ok else None
            except Exception:
                size = None
            print(f"  section {sid:#010x} at {off:#06x}: codec {codec}, {cs} -> {us} B"
                  f"{'' if ok else ' (header not understood)'}{'' if size is None or size == us else ' size?'}")


def cmd_rekey(src, sample, out):
    s = open(src, "rb").read()
    plain = plain_body(s)
    key, a = account_key(open(sample, "rb").read())
    data = s[:HDR] + xor(plain, key, a, len(plain))
    assert plain_body(data) == plain
    open(out, "wb").write(data)
    print(f"{out}: {len(data)} B, re-keyed for the account of {sample}")


if __name__ == "__main__":
    if len(sys.argv) >= 3 and sys.argv[1] == "info":
        cmd_info(sys.argv[2:])
    elif len(sys.argv) == 5 and sys.argv[1] == "rekey":
        cmd_rekey(*sys.argv[2:])
    else:
        sys.exit(__doc__ or "usage: savegame.py info <save>... | rekey <src> <sample> <out>")
