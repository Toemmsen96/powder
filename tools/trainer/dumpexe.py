# Dump a running module's unpacked image to a PE file a disassembler can load. Steep's code is VMProtect-packed on
# disk and only unpacked in memory, so static analysis needs this dump. Read-only on the game.
#   python3 dumpexe.py <pid> [module=steep.exe] [out=<module>.dump.exe]
#
# The image is copied as mapped (unreadable pages become zeros), and the section headers are rewritten so that
# file offsets equal virtual addresses (PointerToRawData = VirtualAddress, SizeOfRawData = the section's span,
# FileAlignment = SectionAlignment): the file is laid out like memory. ImageBase is set to where it was loaded, so
# addresses in the dump match the live process (and RVAs match steep.exe+RVA everywhere else in tools/). Imports
# are already resolved (the IAT holds absolute addresses), which is fine for reading code.
import os, struct, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from watch import modules  # noqa: E402


def main():
    pid = int(sys.argv[1])
    name = sys.argv[2] if len(sys.argv) > 2 else "steep.exe"
    out = sys.argv[3] if len(sys.argv) > 3 else name.rsplit(".", 1)[0] + ".dump.exe"
    m = next(((lo, hi) for lo, hi, p in modules(pid) if os.path.basename(p).lower() == name.lower()), None)
    if not m:
        sys.exit(f"{name} not loaded in {pid}")
    base, end = m
    mem = open(f"/proc/{pid}/mem", "rb", 0)
    img = bytearray(end - base)
    bad = 0
    for off in range(0, len(img), 0x1000):
        try:
            mem.seek(base + off)
            page = mem.read(0x1000)
        except OSError:
            page = b""
        if len(page) == 0x1000:
            img[off:off + 0x1000] = page
        else:
            bad += 1
    pe = struct.unpack_from("<I", img, 0x3C)[0]
    nsec = struct.unpack_from("<H", img, pe + 6)[0]
    opt = pe + 24
    optsize = struct.unpack_from("<H", img, pe + 20)[0]
    salign = struct.unpack_from("<I", img, opt + 32)[0]
    struct.pack_into("<II", img, pe + 12, 0, 0)                   # no COFF symbol table (its file offset is stale)
    struct.pack_into("<I", img, opt + 36, salign)                 # FileAlignment = SectionAlignment
    struct.pack_into("<Q", img, opt + 24, base)                   # ImageBase = the live base
    secs = []
    for i in range(nsec):
        s = opt + optsize + 40 * i
        vsize, va = struct.unpack_from("<II", img, s + 8)
        span = ((max(vsize, 1) + salign - 1) // salign) * salign
        struct.pack_into("<II", img, s + 16, min(span, len(img) - va), va)   # SizeOfRawData, PointerToRawData
        if img[s] == ord("/"):                                     # "/N" = a name in the dropped string table
            img[s:s + 8] = f".s{i}".encode().ljust(8, b"\0")
        secs.append((img[s:s + 8].rstrip(b"\0").decode("latin1"), va, vsize))
    open(out, "wb").write(img)
    print(f"{name} at {base:#x}: {len(img) / 1048576:.1f} MiB -> {out} ({bad} unreadable pages zeroed)")
    for n, va, vs in secs:
        print(f"  {n:<10} rva {va:#010x} size {vs:#x}")


if __name__ == "__main__":
    main()
