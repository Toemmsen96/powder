# Pointer scan: which memory points at a struct (and what points at those), up to a static address in a module
# image - a path that survives relaunches, unlike the struct's heap address.
#   python3 ptrscan.py <pid> <hex struct addr> [levels=3] [max back-offset=0x800] [out.json]
#
# Level 1 = every 8-aligned qword in writable memory whose value is in [target - maxoff, target]; each hit's
# "object" is unknown, so level 2 searches pointers into [hit - maxoff, hit], and so on. Hits inside a module
# image (steep.exe+RVA) are static roots: print the chain (root -> +off -> ... -> struct).
import os, struct, sys
import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scan  # noqa: E402
from watch import modules  # noqa: E402

CHUNK = 64 * 1024 * 1024


def find_pointers(pid: int, windows: list[tuple[int, int]]) -> list[tuple[int, int]]:
    """(holder address, value) for each qword whose value lies in one of the [lo, hi] windows."""
    ws = sorted(windows)
    los = np.array([w[0] for w in ws], dtype=np.uint64)
    his = np.array([w[1] for w in ws], dtype=np.uint64)
    gmin, gmax = los.min(), his.max()
    out = []
    mem = open(f"/proc/{pid}/mem", "rb", 0)
    for start, end in scan.regions(pid):
        off = start
        while off < end:
            n = min(CHUNK, end - off)
            try:
                mem.seek(off); buf = mem.read(n)
            except OSError:
                break
            if not buf:
                break
            v = np.frombuffer(buf, dtype=np.uint64, count=len(buf) // 8)
            idx = np.nonzero((v >= gmin) & (v <= gmax))[0]
            if idx.size:
                vals = v[idx]
                k = np.searchsorted(los, vals, side="right") - 1
                ok = (k >= 0) & (vals <= his[np.clip(k, 0, None)])
                for i, val in zip(idx[ok], vals[ok]):
                    out.append((off + int(i) * 8, int(val)))
            off += len(buf)
    return out


def main():
    pid, target = int(sys.argv[1]), int(sys.argv[2], 16)
    levels = int(sys.argv[3]) if len(sys.argv) > 3 else 3
    maxoff = int(sys.argv[4], 0) if len(sys.argv) > 4 else 0x800
    images = [(lo, hi, os.path.basename(p)) for lo, hi, p in modules(pid) if p.lower().endswith((".exe", ".dll"))]

    def static(a):
        for lo, hi, name in images:
            if lo <= a < hi:
                return f"{name}+{a - lo:#x}"
        return None

    frontier = {target: []}          # address -> chain of (holder, offset) down to the target
    seen = {target}
    roots = []
    for level in range(1, levels + 1):
        windows = [(a - maxoff, a) for a in frontier]
        hits = find_pointers(pid, windows)
        nxt = {}
        keys = np.array(sorted(frontier), dtype=np.uint64)
        for holder, val in hits:
            # which frontier object val points into: the nearest at or above it
            obj = int(keys[np.searchsorted(keys, np.uint64(val), side="left")])
            chain = [(holder, obj - val)] + frontier[obj]
            s = static(holder)
            if s:
                roots.append((s, chain))
            elif holder not in seen and len(nxt) < 20000:
                nxt[holder] = chain
                seen.add(holder)
        print(f"level {level}: {len(hits)} pointers, {len(roots)} static roots so far, next frontier {len(nxt)}",
              flush=True)
        frontier = nxt
        if not frontier:
            break
    # struct fields are 8-aligned; an odd offset is a stray value that happens to point near the target
    roots = [(s, c) for s, c in roots if all(off % 8 == 0 for _h, off in c)]
    roots.sort(key=lambda r: (len(r[1]), sum(off for _h, off in r[1])))
    out = sys.argv[5] if len(sys.argv) > 5 else None
    if out:
        import json
        json.dump([{"root": s, "offsets": [off for _h, off in c]} for s, c in roots], open(out, "w"), indent=0)
    print(f"{len(roots)} aligned static chains" + (f" -> {out}" if out else ""))
    for s, chain in roots[:40]:
        # [[root] + o1] + o2 ... = the struct: each hop reads a pointer and adds the offset
        print(f"[{s}] " + " ".join(f"-> +{off:#x}" for _h, off in chain))


if __name__ == "__main__":
    main()
