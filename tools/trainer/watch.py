# "What writes this address?" from OUTSIDE the game: a Linux perf hardware watchpoint (perf_event_open,
# PERF_TYPE_BREAKPOINT) on every thread of the game. The kernel traps the write and records the instruction
# pointer + registers; the game sees no exception and no debug register (Wine's GetThreadContext does not show
# perf's breakpoints, so VMProtect cannot find or clear them), unlike the in-process page-guard access-finder
# (crashes on hot pages) and SetThreadContext debug registers (VMProtect clears them).
#
#   python3 watch.py <pid> <hex addr> [seconds=10] [len=4] [w|rw]     who writes this address
#   python3 watch.py <pid> <module+rva> [seconds=10]                  what this instruction writes (each launch)
#
# Address mode prints each writing instruction (module+RVA, decoded with objdump), hit count, the threads, and
# its base register: base+disp = the watched field. Instruction mode (an execute breakpoint on a writer found
# before; the RVA is stable across launches) prints the addresses it wrote: the position, found in seconds.
# Needs: same user as the game, kernel.perf_event_paranoid <= 2, objdump (binutils).
import ctypes, mmap, os, re, struct, subprocess, sys, tempfile, time
from collections import defaultdict

PERF_TYPE_BREAKPOINT, HW_BP_W, HW_BP_RW, HW_BP_X = 5, 2, 3, 4
SAMPLE_IP, SAMPLE_TID, SAMPLE_REGS_USER = 1, 2, 0x1000
REGS = ["rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "rip", "flags"] + [f"r{i}" for i in range(8, 16)]
REG_BITS = list(range(10)) + list(range(16, 24))          # perf_regs x86_64: AX..FLAGS, R8..R15
REG_MASK = sum(1 << b for b in REG_BITS)
NR_perf_event_open = 298
PAGE = mmap.PAGESIZE
DATA_PAGES = 16

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long


def perf_open(tid: int, addr: int, length: int, bp_type: int) -> int:
    attr = bytearray(112)                                   # PERF_ATTR_SIZE_VER5
    flags = (1 << 0) | (1 << 5) | (1 << 6)                  # disabled, exclude_kernel, exclude_hv
    struct.pack_into("<IIQQQQQII QQQQ", attr, 0, PERF_TYPE_BREAKPOINT, 112, 0, 1,
                     SAMPLE_IP | SAMPLE_TID | SAMPLE_REGS_USER, 0, flags, 1, bp_type, addr, length, 0, REG_MASK)
    buf = (ctypes.c_char * len(attr)).from_buffer(attr)
    fd = libc.syscall(NR_perf_event_open, buf, tid, -1, -1, 0)
    if fd < 0:
        raise OSError(ctypes.get_errno(), f"perf_event_open(tid {tid}): {os.strerror(ctypes.get_errno())}")
    return fd


class Ring:
    def __init__(self, fd: int):
        self.fd = fd
        self.m = mmap.mmap(fd, (1 + DATA_PAGES) * PAGE, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
        self.size = DATA_PAGES * PAGE

    def records(self):
        head = struct.unpack_from("<Q", self.m, 1024)[0]
        tail = struct.unpack_from("<Q", self.m, 1032)[0]
        while tail < head:
            off = PAGE + tail % self.size
            hdr = self._read(off, 8)
            rtype, _misc, size = struct.unpack("<IHH", hdr)
            yield rtype, self._read(off + 8, size - 8)
            tail += size
        struct.pack_into("<Q", self.m, 1032, tail)

    def _read(self, off: int, n: int) -> bytes:
        rel = off - PAGE
        if rel + n <= self.size:
            return self.m[off:off + n]
        first = self.size - rel
        return self.m[off:off + first] + self.m[PAGE:PAGE + n - first]


def modules(pid: int) -> list[tuple[int, int, str]]:
    """(start, end, path) of file mappings, merged per path; a module's base is its lowest mapping."""
    spans: dict[str, list[int]] = {}
    for line in open(f"/proc/{pid}/maps"):
        parts = line.split(maxsplit=5)
        if len(parts) < 6 or not parts[5].strip().startswith("/"):
            continue
        lo, hi = (int(x, 16) for x in parts[0].split("-"))
        path = parts[5].strip()
        s = spans.setdefault(path, [lo, hi])
        s[0], s[1] = min(s[0], lo), max(s[1], hi)
    return [(lo, hi, p) for p, (lo, hi) in spans.items()]


def where(mods, addr: int) -> str:
    for lo, hi, path in mods:
        if lo <= addr < hi:
            return f"{os.path.basename(path)}+{addr - lo:#x}"
    return f"(anon){addr:#x}"


MEM = re.compile(r"\[([^\]]+)\]")


def eff_addr(operand: str, regs: dict) -> int | None:
    """Effective address of an Intel-syntax memory operand like 'rbx+rcx*4+0x40' with the sampled registers."""
    total = 0
    for sign, term in re.findall(r"([+-]?)\s*([\w*]+)", operand.replace(" ", "")):
        if "*" in term:
            r, scale = term.split("*")
            v = regs.get(r)
            if v is None:
                return None
            v *= int(scale, 0)
        elif term in regs:
            v = regs[term]
        elif re.fullmatch(r"0x[0-9a-f]+|\d+", term):
            v = int(term, 0)
        else:
            return None
        total += -v if sign == "-" else v
    return total & 0xFFFFFFFFFFFFFFFF


def decode_before(mem, next_ip: int, regs: dict, watched: int, length: int):
    """The instruction that ends at next_ip (a watchpoint traps after the write). Try each start in the 15 bytes
    before it; keep a decoding whose memory operand, with the sampled registers, hits the watched range."""
    try:
        mem.seek(next_ip - 15)
        code = mem.read(15)
    except OSError:
        return None
    best = None
    with tempfile.NamedTemporaryFile(suffix=".bin") as tf:
        for start in range(15):
            tf.seek(0); tf.truncate(); tf.write(code[start:]); tf.flush()
            out = subprocess.run(["objdump", "-D", "-b", "binary", "-m", "i386:x86-64", "-M", "intel",
                                  f"--adjust-vma={next_ip - 15 + start:#x}", tf.name],
                                 capture_output=True, text=True).stdout
            lines = [l for l in out.splitlines() if re.match(r"\s+[0-9a-f]+:\t", l)]
            if len(lines) != 1:            # must be exactly one instruction ending at next_ip
                continue
            text = lines[0].split("\t")[-1].strip()
            if "(bad)" in text:
                continue
            for op in MEM.findall(text):
                op = re.sub(r"^\w+:", "", op)   # drop a segment prefix
                ea = eff_addr(op, {**regs, "rip": next_ip})
                if ea is not None and ea <= watched < ea + 64 and watched + length > ea:
                    return next_ip - 15 + start, text, op
            if best is None:
                best = (next_ip - 15 + start, text, None)
    return best


def sample(pid: int, addr: int, length: int, bp_type: int, secs: float):
    """Yield (ip, tid, regs) of each hit of a breakpoint on every thread of pid, for secs seconds."""
    rings: dict[int, Ring] = {}

    def attach_new():
        for t in os.listdir(f"/proc/{pid}/task"):
            tid = int(t)
            if tid in rings:
                continue
            try:
                fd = perf_open(tid, addr, length, bp_type)
            except OSError as e:
                if e.errno == 3:            # thread gone
                    continue
                raise
            rings[tid] = Ring(fd)
            fcntl_ioctl(fd, 0x2400)          # PERF_EVENT_IOC_ENABLE

    attach_new()
    print(f"watching {addr:#x} on {len(rings)} threads for {secs:.0f}s ...", flush=True)
    end, nregs, last_scan = time.time() + secs, len(REG_BITS), time.time()
    try:
        while time.time() < end:
            for ring in list(rings.values()):
                for rtype, body in ring.records():
                    if rtype != 9:           # PERF_RECORD_SAMPLE
                        continue
                    ip, _pid, stid, abi = struct.unpack_from("<QIIQ", body, 0)
                    regs = dict(zip(REGS, struct.unpack_from(f"<{nregs}Q", body, 24))) if abi else {}
                    yield ip, stid, regs
            time.sleep(0.02)
            if time.time() - last_scan > 1:
                attach_new(); last_scan = time.time()
    finally:
        for ring in rings.values():
            ring.m.close(); os.close(ring.fd)


def open_mem(pid: int):
    try:
        return open(f"/proc/{pid}/mem", "rb", 0)
    except OSError as e:
        print(f"(cannot read the game's memory: {e})")
        return None


def watch_writes(pid: int, addr: int, secs: float, length: int, bp_type: int):
    if addr % length:
        sys.exit(f"address must be aligned to len {length}")
    hits = defaultdict(lambda: {"n": 0, "tids": set(), "regs": None})
    for ip, tid, regs in sample(pid, addr, length, bp_type, secs):
        h = hits[ip]
        h["n"] += 1; h["tids"].add(tid); h["regs"] = regs
    if not hits:
        print("no writes seen (is the address right? move the player while watching)")
        return
    mods, mem = modules(pid), open_mem(pid)
    print(f"\n{'hits':>6}  {'instruction (start)':<34} {'decoded':<46} base / struct")
    for ip, h in sorted(hits.items(), key=lambda kv: -kv[1]["n"]):
        d = decode_before(mem, ip, h["regs"], addr, length) if mem else None
        start, text, op = d if d else (ip, "?", None)
        base = ""
        if op:
            m = re.match(r"(\w+)", op.replace(" ", ""))
            if m and m.group(1) in h["regs"] and m.group(1) != "rip":
                bv = h["regs"][m.group(1)]
                base = f"{m.group(1)}={bv:#x}  field +{addr - bv:#x}"
        print(f"{h['n']:>6}  {where(mods, start):<34} {text:<46} {base}  tids={len(h['tids'])}")


def watch_exec(pid: int, spec: str, secs: float):
    """Execute breakpoint on an instruction (a writer found before; its RVA is the same every launch): the
    addresses its memory operand touched, i.e. the position of each body it writes, without a memory scan."""
    name, rva = spec.rsplit("+", 1)
    base = next((lo for lo, _hi, p in modules(pid) if os.path.basename(p).lower() == name.lower()), None)
    if base is None:
        sys.exit(f"module {name} not mapped")
    ip = base + int(rva, 16)
    mem = open_mem(pid)
    if mem is None:
        sys.exit(1)
    mem.seek(ip)
    code = mem.read(16)
    with tempfile.NamedTemporaryFile(suffix=".bin") as tf:
        tf.write(code); tf.flush()
        out = subprocess.run(["objdump", "-D", "-b", "binary", "-m", "i386:x86-64", "-M", "intel",
                              f"--adjust-vma={ip:#x}", tf.name], capture_output=True, text=True).stdout
    line = next(l for l in out.splitlines() if re.match(r"\s+[0-9a-f]+:\t", l))
    nbytes, text = len(line.split("\t")[1].split()), line.split("\t")[-1].strip()
    ops = MEM.findall(text)
    if not ops:
        sys.exit(f"'{text}' has no memory operand")
    op = re.sub(r"^\w+:", "", ops[0])
    print(f"{spec}: {text}")
    eas = defaultdict(int)
    for _ip, _tid, regs in sample(pid, ip, 8, HW_BP_X, secs):
        ea = eff_addr(op, {**regs, "rip": ip + nbytes})
        if ea is not None:
            eas[ea] += 1
    if not eas:
        print("not executed (is the player moving?)")
        return
    print(f"\n{'hits':>6}  {'address':<16} float3 there")
    for ea, n in sorted(eas.items(), key=lambda kv: -kv[1])[:40]:
        mem.seek(ea)
        b = mem.read(12)
        v = struct.unpack("<3f", b) if len(b) == 12 else ()
        print(f"{n:>6}  {ea:#016x}  {tuple(round(x, 2) for x in v)}")


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: watch.py <pid> <hex addr> [seconds=10] [len=4] [w|rw]   (who writes this address)\n"
                 "       watch.py <pid> <module+rva> [seconds=10]               (what this instruction writes)")
    pid = int(sys.argv[1])
    secs = float(sys.argv[3]) if len(sys.argv) > 3 else 10.0
    if "+" in sys.argv[2]:
        watch_exec(pid, sys.argv[2], secs)
    else:
        length = int(sys.argv[4]) if len(sys.argv) > 4 else 4
        watch_writes(pid, int(sys.argv[2], 16), secs, length,
                     HW_BP_RW if len(sys.argv) > 5 and sys.argv[5] == "rw" else HW_BP_W)


def fcntl_ioctl(fd: int, req: int):
    if libc.ioctl(fd, req, 0) < 0:
        raise OSError(ctypes.get_errno(), "ioctl")


if __name__ == "__main__":
    main()
