# Noclip by hooking: make the game's own writes to one field (the player's position) no-ops, from OUTSIDE the
# process, so physics/collision can no longer move the player and the overlay's Fly (or poke) moves it freely.
#
#   python3 skiphook.py <pid> install <module+rva> [<module+rva> ...]   # the writers watch.py found
#   python3 skiphook.py <pid> on <hex addr> [size=12]                    # skip writes into [addr, addr+size)
#   python3 skiphook.py <pid> off                                        # writes go through again
#   python3 skiphook.py <pid> remove                                     # restore the original code
#   python3 skiphook.py <pid> status
#
# Each writer instruction gets a 5-byte `jmp` to a cave in its module's section slack (the zero tail of the last
# page of an executable section). The cave computes the write's effective address with a `lea` of the same
# operand and skips the write only if it falls in [lo, hi); everything else (other bodies, NPCs) runs as before.
# `on`/`off` only rewrite lo/hi in the cave. Patching goes through /proc/<pid>/mem, which writes read-only code
# pages (VMProtect blocks the game's own VirtualProtect, not this). State: $XDG_RUNTIME_DIR/powder-skiphook-<pid>.json.
import json, os, re, struct, subprocess, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from watch import modules  # noqa: E402

BRANCH = re.compile(r"^(j\w+|call|ret\w*|loop\w*|jmp)\b")
TARGET = re.compile(r"#\s*(0x[0-9a-f]+)|^(?:j\w+|call|jmp)\s+(0x[0-9a-f]+)")


def state_path(pid: int) -> str:
    return os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), f"powder-skiphook-{pid}.json")


def load(pid: int) -> dict:
    try:
        return json.load(open(state_path(pid)))
    except FileNotFoundError:
        return {"hooks": [], "cave_used": {}}


def save(pid: int, st: dict):
    json.dump(st, open(state_path(pid), "w"), indent=1)


class Mem:
    def __init__(self, pid: int):
        self.f = open(f"/proc/{pid}/mem", "r+b", 0)

    def read(self, a: int, n: int) -> bytes:
        self.f.seek(a)
        return self.f.read(n)

    def write(self, a: int, b: bytes):
        self.f.seek(a)
        if self.f.write(b) != len(b):
            raise OSError(f"short write at {a:#x}")


def disasm(code: bytes, vma: int) -> list[tuple[int, int, str]]:
    """(address, length, intel text) of each instruction objdump decodes in code."""
    with tempfile.NamedTemporaryFile(suffix=".bin") as tf:
        tf.write(code); tf.flush()
        out = subprocess.run(["objdump", "-D", "-b", "binary", "-m", "i386:x86-64", "-M", "intel",
                              f"--adjust-vma={vma:#x}", tf.name], capture_output=True, text=True, check=True).stdout
    rows = []
    for line in out.splitlines():
        m = re.match(r"\s+([0-9a-f]+):\t([0-9a-f ]+?)\s*\t(.*)", line)
        if m:
            rows.append((int(m.group(1), 16), len(m.group(2).split()), m.group(3).strip()))
        elif rows and re.match(r"\s+[0-9a-f]+:\t[0-9a-f ]+$", line):      # objdump wraps long instructions
            a, n, t = rows[-1]
            rows[-1] = (a, n + len(line.split("\t")[1].split()), t)
    return rows


def assemble(src: str, org: int) -> tuple[bytes, dict[str, int]]:
    """Assemble Intel-syntax src linked at absolute address org: (bytes, {label: address})."""
    with tempfile.TemporaryDirectory() as d:
        s, o, e, b = (os.path.join(d, x) for x in ("c.s", "c.o", "c.elf", "c.bin"))
        open(s, "w").write(".intel_syntax noprefix\n.globl _start\n_start:\n" + src)
        subprocess.run(["as", "--64", "-o", o, s], check=True)
        subprocess.run(["ld", "-m", "elf_x86_64", f"-Ttext={org:#x}", "-o", e, o], check=True)
        subprocess.run(["objcopy", "-O", "binary", "-j", ".text", e, b], check=True)
        syms = {}
        for line in subprocess.run(["nm", e], capture_output=True, text=True, check=True).stdout.splitlines():
            parts = line.split()
            if len(parts) == 3:
                syms[parts[2]] = int(parts[0], 16)
        return open(b, "rb").read(), syms


def relocate(ins: bytes, old: int, new: int, text: str) -> bytes:
    """An instruction copied from old to new: fix its rel32 (rip-relative operand, call/jmp/jcc rel32)."""
    m = TARGET.search(text)
    if not m:
        return ins
    if BRANCH.match(text) and not (ins[0] in (0xE8, 0xE9) or (ins[0] == 0x0F and 0x80 <= ins[1] <= 0x8F)):
        raise ValueError(f"cannot relocate short branch '{text}'")
    target = int(m.group(1) or m.group(2), 16)
    old_rel = struct.pack("<i", target - (old + len(ins)))
    for i in range(len(ins) - 4, -1, -1):          # disp32/rel32 is followed by at most an immediate
        if ins[i:i + 4] == old_rel:
            rel = target - (new + len(ins))
            if not -2**31 <= rel < 2**31:
                raise ValueError(f"'{text}' target out of rel32 range from the cave")
            return ins[:i] + struct.pack("<i", rel) + ins[i + 4:]
    raise ValueError(f"cannot find the displacement of '{text}'")


def resolve(pid: int, spec: str) -> tuple[int, int]:
    """'steep.exe+0x1234' -> (absolute address, module base)."""
    name, rva = spec.rsplit("+", 1)
    for lo, _hi, path in modules(pid):
        if os.path.basename(path).lower() == name.lower():
            return lo + int(rva, 16), lo
    sys.exit(f"module {name} not mapped in {pid}")


def slack(mem: Mem, base: int, pid: int) -> list[tuple[int, int]]:
    """(start, size) of the zero tail of each executable section's last page that is mapped executable."""
    hdr = mem.read(base, 0x1000)
    pe = struct.unpack_from("<I", hdr, 0x3C)[0]
    nsec = struct.unpack_from("<H", hdr, pe + 6)[0]
    opt = struct.unpack_from("<H", hdr, pe + 20)[0]
    execs = []
    for line in open(f"/proc/{pid}/maps"):
        rng, perms = line.split()[:2]
        if "x" in perms:
            execs.append(tuple(int(x, 16) for x in rng.split("-")))
    out = []
    for i in range(nsec):
        off = pe + 24 + opt + 40 * i
        vsize, vaddr = struct.unpack_from("<II", hdr, off + 8)
        chars = struct.unpack_from("<I", hdr, off + 36)[0]
        if not chars & 0x20000000 or not vsize:      # IMAGE_SCN_MEM_EXECUTE
            continue
        start = (base + vaddr + vsize + 15) & ~15
        end = (start + 0xFFF) & ~0xFFF
        if end - start >= 128 and any(lo <= start and end <= hi for lo, hi in execs):
            out.append((start, end - start))
    return out


def install(pid: int, specs: list[str]):
    mem, st = Mem(pid), load(pid)
    for spec in specs:
        site, base = resolve(pid, spec)
        if any(h["site"] == site for h in st["hooks"]):
            print(f"{spec}: already hooked"); continue
        code = mem.read(site, 32)
        if code[0] == 0xE9:
            sys.exit(f"{spec}: starts with a jmp already (hooked by someone else?)")
        rows, covered = [], 0
        for a, n, text in disasm(code, site):
            rows.append((a, n, text)); covered += n
            if covered >= 5:
                break
        writer = rows[0]
        mops = re.findall(r"\[([^\]]+)\]", writer[2])
        if not mops:
            sys.exit(f"{spec}: '{writer[2]}' has no memory operand")
        if "rip" in mops[0]:
            sys.exit(f"{spec}: the writer is rip-relative (a global, not a struct field)")
        if any(re.match(r"^(ret|loop|j\w*cxz)", t) for _a, _n, t in rows):
            sys.exit(f"{spec}: a ret/loop within the 5 patched bytes; pick another writer")
        if re.search(r"\b[re]?sp\b", mops[0]):
            sys.exit(f"{spec}: stack-relative writer, not a struct field")
        orig = code[:covered]
        wlen = writer[1]
        # pick a cave in this module's slack
        caves = slack(mem, base, pid)
        used = st["cave_used"]
        cave = None
        for cs, size in caves:
            u = used.get(str(cs), 0)
            while size - u >= 128 and mem.read(cs + u, 128) != bytes(128):   # in use by someone else
                u += 128
            if size - u >= 128:
                cave = cs + u; used[str(cs)] = u + 128; break
        if cave is None:
            sys.exit(f"{spec}: no executable slack left in the module for a cave")
        op = re.sub(r"^\w+ PTR ", "", mops[0]) if "PTR" in mops[0] else mops[0]
        src = f"""
            pushfq
            push rax
            lea rax, [{op}]
            cmp rax, qword ptr [rip+lo]
            jb 1f
            cmp rax, qword ptr [rip+hi]
            jae 1f
            pop rax
            popfq
            jmp 2f
        1:  pop rax
            popfq
        W:  .byte {", ".join(str(b) for b in orig[:wlen])}
        2:
        R:  {(".byte " + ", ".join(str(b) for b in orig[wlen:])) if covered > wlen else ""}
            jmp qword ptr [rip+back]
        back: .quad {site + covered}
        lo: .quad 0
        hi: .quad 0
        """
        # lea's operand must not depend on rsp (push moved it); checked above
        blob, syms = assemble(src, cave)
        if len(blob) > 128:
            sys.exit(f"{spec}: cave too large ({len(blob)})")
        blob = bytearray(blob)
        new = syms["W"]
        try:
            for a, n, text in rows:
                off = new - cave
                blob[off:off + n] = relocate(bytes(blob[off:off + n]), a, new, text)
                new += n
                if new == syms["W"] + wlen:
                    new = syms["R"]
        except ValueError as e:
            sys.exit(f"{spec}: {e}")
        blob = bytes(blob)
        lo_off = syms["lo"] - cave
        mem.write(cave, blob)
        rel = cave - (site + 5)
        if not -2**31 <= rel < 2**31:
            sys.exit(f"{spec}: cave out of rel32 range")
        mem.write(site, b"\xE9" + struct.pack("<i", rel) + b"\x90" * (covered - 5))
        st["hooks"].append({"spec": spec, "site": site, "orig": orig.hex(), "cave": cave, "lo": cave + lo_off,
                            "writer": writer[2]})
        save(pid, st)
        print(f"{spec}: hooked '{writer[2]}' (+{covered - wlen} relocated bytes) -> cave {cave:#x}")


def set_range(pid: int, lo: int, hi: int):
    mem, st = Mem(pid), load(pid)
    if not st["hooks"]:
        sys.exit("no hooks installed (run install first)")
    for h in st["hooks"]:
        mem.write(h["lo"], struct.pack("<QQ", lo, hi))
    print(f"{len(st['hooks'])} hook(s): skip writes into [{lo:#x}, {hi:#x})" if hi > lo else "hooks idle (off)")


def remove(pid: int):
    mem, st = Mem(pid), load(pid)
    for h in st["hooks"]:
        mem.write(h["lo"], struct.pack("<QQ", 0, 0))
        mem.write(h["site"], bytes.fromhex(h["orig"]))
        print(f"{h['spec']}: restored")
    # caves are left in place (zeroing them could pull code from under a thread still inside one)
    os.unlink(state_path(pid)) if os.path.exists(state_path(pid)) else None


def status(pid: int):
    mem, st = Mem(pid), load(pid)
    for h in st["hooks"]:
        lo, hi = struct.unpack("<QQ", mem.read(h["lo"], 16))
        live = mem.read(h["site"], 1) == b"\xE9"
        print(f"{h['spec']:<28} {h['writer']:<44} {'patched' if live else 'NOT patched'}  "
              f"{f'skip [{lo:#x},{hi:#x})' if hi > lo else 'off'}")


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: skiphook.py <pid> install <mod+rva>... | on <addr> [size] | off | remove | status")
    pid, cmd = int(sys.argv[1]), sys.argv[2]
    if cmd == "install":
        install(pid, sys.argv[3:])
    elif cmd == "on":
        a = int(sys.argv[3], 16)
        set_range(pid, a, a + (int(sys.argv[4], 0) if len(sys.argv) > 4 else 12))
    elif cmd == "off":
        set_range(pid, 0, 0)
    elif cmd == "remove":
        remove(pid)
    elif cmd == "status":
        status(pid)
    else:
        sys.exit(f"unknown command {cmd}")


if __name__ == "__main__":
    main()
