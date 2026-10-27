#!/usr/bin/env python3
# External float memory scanner for finding the player position in a running (Wine/Proton) game, via
# /proc/<pid>/mem. No cap, numpy-accelerated. Run with system python3 (has numpy).
#
#   snapshot: python3 scan.py snapshot <pid> <out.npz>
#   narrow:   python3 scan.py narrow <pid> <in.npz> <out.npz> <mode> [arg]
#               modes: changed [min]   unchanged [tol]   increased   decreased
#                      near <value> [tol]   delta <d> [tol]   range <lo> <hi>
#   show:     python3 scan.py show <in.npz> [n]     (lists matches + contiguous X/Y/Z triples)
import sys, numpy as np

CHUNK = 64 * 1024 * 1024
PLAUS_HI = 1e7          # world coords are finite and not absurd
PLAUS_LO = 1e-3

def regions(pid):
    out = []
    for line in open(f"/proc/{pid}/maps"):
        parts = line.split()
        if len(parts) < 5:
            continue
        rng, perms = parts[0], parts[1]
        path = parts[5] if len(parts) >= 6 else ""
        if not perms.startswith("rw"):
            continue
        if path in ("[vvar]", "[vsyscall]", "[vdso]") or path.startswith("/dev"):
            continue
        a, b = rng.split("-")
        out.append((int(a, 16), int(b, 16)))
    return out

def scan(pid):
    """Return (addrs uint64, vals float32) for plausible-coordinate floats across writable memory."""
    addrs_all, vals_all = [], []
    mem = open(f"/proc/{pid}/mem", "rb", 0)
    for start, end in regions(pid):
        off = start
        while off < end:
            n = min(CHUNK, end - off)
            n -= n % 4
            if n <= 0:
                break
            try:
                mem.seek(off)
                buf = mem.read(n)
            except (OSError, ValueError):
                break
            if not buf:
                break
            got = len(buf) - (len(buf) % 4)
            v = np.frombuffer(buf, dtype=np.float32, count=got // 4)
            av = np.abs(v)
            mask = np.isfinite(v) & (av > PLAUS_LO) & (av < PLAUS_HI)
            idx = np.nonzero(mask)[0]
            if idx.size:
                addrs_all.append(off + idx.astype(np.uint64) * 4)
                vals_all.append(v[idx])
            off += got
    mem.close()
    if not addrs_all:
        return np.empty(0, np.uint64), np.empty(0, np.float32)
    return np.concatenate(addrs_all), np.concatenate(vals_all)

def cmd_snapshot(pid, out):
    a, v = scan(int(pid))
    np.savez(out, addr=a, val=v)
    print(f"snapshot: {a.size} plausible floats across writable memory -> {out}")

def cmd_narrow(pid, inf, out, mode, args):
    d = np.load(inf)
    old_a, old_v = d["addr"], d["val"]
    new_a, new_v = scan(int(pid))
    # intersect by address
    common, i_old, i_new = np.intersect1d(old_a, new_a, assume_unique=False, return_indices=True)
    ov, nv = old_v[i_old], new_v[i_new]
    if mode == "changed":
        thr = float(args[0]) if args else 0.5
        m = np.abs(nv - ov) > thr
    elif mode == "unchanged":
        tol = float(args[0]) if args else 1e-3
        m = np.abs(nv - ov) <= tol
    elif mode == "increased":
        m = nv > ov
    elif mode == "decreased":
        m = nv < ov
    elif mode == "near":
        val = float(args[0]); tol = float(args[1]) if len(args) > 1 else 1.0
        m = np.abs(nv - val) <= tol
    elif mode == "delta":
        dd = float(args[0]); tol = float(args[1]) if len(args) > 1 else 0.5
        m = np.abs((nv - ov) - dd) <= tol
    elif mode == "range":
        lo, hi = float(args[0]), float(args[1])
        m = (nv >= lo) & (nv <= hi)
    else:
        print(f"unknown mode {mode}"); sys.exit(2)
    np.savez(out, addr=common[m], val=nv[m])
    print(f"narrow {mode}: {old_a.size} -> {int(m.sum())} matches -> {out}")

def cmd_show(inf, n):
    d = np.load(inf)
    a, v = d["addr"], d["val"]
    order = np.argsort(a)
    a, v = a[order], v[order]
    print(f"{a.size} matches")
    # contiguous X/Y/Z: an address followed by +4 and +8 (a float3), likely a position struct
    s = set(int(x) for x in a)
    triples = [int(x) for x in a if (int(x) + 4) in s and (int(x) + 8) in s]
    if triples:
        print(f"-- {len(triples)} candidate float3 struct(s) (addr, +4, +8 all present) --")
        vmap = {int(a[i]): float(v[i]) for i in range(a.size)}
        for t in triples[:40]:
            print(f"  0x{t:x}: ({vmap[t]:.3f}, {vmap[t+4]:.3f}, {vmap[t+8]:.3f})")
    print(f"-- first {n} matches --")
    for i in range(min(n, a.size)):
        print(f"  0x{int(a[i]):x} = {float(v[i]):.4f}")

if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__); sys.exit(2)
    c = sys.argv[1]
    if c == "snapshot":
        cmd_snapshot(sys.argv[2], sys.argv[3])
    elif c == "narrow":
        cmd_narrow(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6:])
    elif c == "show":
        cmd_show(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 20)
    else:
        print(__doc__); sys.exit(2)
