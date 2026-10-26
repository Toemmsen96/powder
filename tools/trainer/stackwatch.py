# Who writes an address, with the full call stack: a perf write-watchpoint (like watch.py) that also copies the
# thread's user stack at each hit, unwound offline through the unpacked image's exception directory (unwind.py
# over a dumpexe.py dump). Shows the chain physics -> controller -> setter that ends in the write, which is where
# real hooks belong. Read-only on the game.
#   python3 stackwatch.py <pid> <dump.exe> (<hex addr> | --player) [seconds=10] [module=steep.exe]
# --player finds the player's position through native/overlay/steep_trainer.json's pointer chains.
# Turn the overlay's Fly/noclip off while recording: hooked writers run in caves without unwind info.
import ctypes, json, os, struct, sys, time
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import watch as W                  # noqa: E402
from unwind import Image, walk     # noqa: E402

STACK = 16000                      # bytes of user stack per sample (multiple of 8)
PAGES = 16
SAMPLE_STACK_USER = 0x2000


def perf_open(tid: int, addr: int) -> int:
    attr = bytearray(112)
    flags = (1 << 0) | (1 << 5) | (1 << 6)
    struct.pack_into("<IIQQQQQII QQQQ", attr, 0, W.PERF_TYPE_BREAKPOINT, 112, 0, 1,
                     W.SAMPLE_IP | W.SAMPLE_TID | W.SAMPLE_REGS_USER | SAMPLE_STACK_USER, 0, flags, 1, W.HW_BP_W,
                     addr, 4, 0, W.REG_MASK)
    struct.pack_into("<I", attr, 88, STACK)
    buf = (ctypes.c_char * len(attr)).from_buffer(attr)
    fd = W.libc.syscall(W.NR_perf_event_open, buf, tid, -1, -1, 0)
    if fd < 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    return fd


def player_position(pid: int, base: int) -> int:
    spec = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../native/overlay/steep_trainer.json")))
    mem = open(f"/proc/{pid}/mem", "rb", 0)

    def r64(a):
        try:
            mem.seek(a); b = mem.read(8)
            return struct.unpack("<Q", b)[0] if len(b) == 8 else None
        except (OSError, ValueError, OverflowError):
            return None
    votes = Counter()
    for c in spec["position"]["chains"]:
        a = base + int(c["root"], 16)
        for o in c["offsets"]:
            v = r64(a)
            if not v or v < 0x10000:
                a = None; break
            a = v + int(o, 16)
        if a:
            votes[a + int(spec["position"]["offset"], 16)] += 1
    if not votes:
        sys.exit("no chain resolves: is the player in the world?")
    pos, n = votes.most_common(1)[0]
    print(f"player position at {pos:#x} ({n} of {len(spec['position']['chains'])} chains agree)")
    return pos


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    pid, dump = int(sys.argv[1]), sys.argv[2]
    secs = float(sys.argv[4]) if len(sys.argv) > 4 else 10.0
    modname = sys.argv[5] if len(sys.argv) > 5 else "steep.exe"
    mods = W.modules(pid)
    base = next(lo for lo, _hi, p in mods if os.path.basename(p).lower() == modname.lower())
    addr = player_position(pid, base) if sys.argv[3] == "--player" else int(sys.argv[3], 16)
    img = Image(dump, base)
    print(f"{len(img.funcs)} functions with unwind info; watching {addr:#x} for {secs:.0f}s (move!)", flush=True)

    rings, skipped = {}, 0
    for t in os.listdir(f"/proc/{pid}/task"):
        try:
            fd = perf_open(int(t), addr)
        except OSError as e:
            skipped += 1
            continue
        r = W.Ring.__new__(W.Ring)
        r.fd = fd
        import mmap
        r.m = mmap.mmap(fd, (1 + PAGES) * W.PAGE, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
        r.size = PAGES * W.PAGE
        W.fcntl_ioctl(fd, 0x2400)
        rings[int(t)] = r
    print(f"{len(rings)} threads ({skipped} skipped)", flush=True)

    nregs = len(W.REG_BITS)
    stacks = Counter()
    end = time.time() + secs
    while time.time() < end:
        for r in rings.values():
            for rtype, body in r.records():
                if rtype != 9:
                    continue
                ip, _pid, _tid, abi = struct.unpack_from("<QIIQ", body, 0)
                off = 24
                regs = {}
                if abi:
                    regs = dict(zip(W.REGS, struct.unpack_from(f"<{nregs}Q", body, off)))
                    off += 8 * nregs
                size = struct.unpack_from("<Q", body, off)[0]
                data = body[off + 8:off + 8 + size]
                dyn = struct.unpack_from("<Q", body, off + 8 + size)[0] if size else 0
                data = data[:dyn]
                sp = regs.get("rsp", 0)

                def read64(a, sp=sp, data=data):
                    o = a - sp
                    return struct.unpack_from("<Q", data, o)[0] if 0 <= o and o + 8 <= len(data) else None
                regs["rip"] = ip
                frames = walk(img, regs, read64)
                stacks[tuple(frames)] += 1
        time.sleep(0.02)
    for r in rings.values():
        r.m.close(); os.close(r.fd)

    if not stacks:
        print("no writes seen (moving? Fly off?)")
        return
    # group by the stack's code locations (module+rva), most frequent first
    def name(a):
        rva = a - base
        if 0 <= rva < img.size:
            f = img.func(rva)
            return f"{modname}+{rva:#x}" + (f" [fn {f[0]:#x}+{rva - f[0]:#x}]" if f else " [no unwind info]")
        return W.where(mods, a)
    merged = Counter()
    for frames, n in stacks.items():
        merged[tuple(frames)] += n
    for frames, n in merged.most_common(8):
        print(f"\n{n} write(s):")
        for a in frames:
            print("   ", name(a))


if __name__ == "__main__":
    main()
