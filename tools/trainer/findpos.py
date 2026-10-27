#!/usr/bin/env python3
# Fast player-position finder: the position is a large-coordinate float3 the game replicates many times.
# Find the most-replicated large-magnitude float3 in one pass, and collect every address that holds it.
#   python3 findpos.py <pid> [out.npz]
import sys, struct, numpy as np, collections, scan

pid = int(sys.argv[1])
out = sys.argv[2] if len(sys.argv) > 2 else "copies.npz"
MINMAG = 300.0          # world coords are large; skip small/normalized vectors
counter = collections.Counter()
locs = collections.defaultdict(list)
mem = open(f"/proc/{pid}/mem", "rb", 0)

for start, end in scan.regions(pid):
    off = start
    while off < end:
        n = min(64 * 1024 * 1024, end - off)
        n -= n % 4
        if n <= 0:
            break
        try:
            mem.seek(off); buf = mem.read(n)
        except (OSError, ValueError):
            break
        if not buf:
            break
        got = len(buf) - (len(buf) % 4)
        a = np.frombuffer(buf, dtype=np.float32, count=got // 4)
        if a.size < 3:
            off += got; continue
        x, y, z = a[:-2], a[1:-1], a[2:]
        m = (np.abs(x) > MINMAG) & (np.abs(y) > MINMAG) & (np.abs(z) > MINMAG) & \
            np.isfinite(x) & np.isfinite(y) & np.isfinite(z) & (np.abs(x) < 1e7)
        idx = np.nonzero(m)[0]
        for i in idx:
            key = (round(float(x[i])), round(float(y[i])), round(float(z[i])))
            counter[key] += 1
            locs[key].append(off + int(i) * 4)
        off += got
mem.close()

if not counter:
    print("no large-coordinate float3 found"); sys.exit(1)
top = counter.most_common(8)
print("most-replicated large float3 (likely position candidates):")
for k, c in top:
    print(f"  {k}  x{c}")
key = top[0][0]
addrs = sorted(locs[key])
np.savez(out, addr=np.array(addrs, dtype=np.uint64))
print(f"position ~= {key}; {len(addrs)} copies -> {out}")
