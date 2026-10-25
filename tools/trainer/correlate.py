#!/usr/bin/env python3
# Isolate the true player-position copies by movement-delta correlation.
#   A: python3 correlate.py A <pid>          # snapshot candidate addrs + values (stand still)
#   B: python3 correlate.py B <pid>          # after moving: keep addrs that moved by the dominant exact vector
import sys, struct, numpy as np, collections, scan

def all_pos_addrs(pid):
    """addresses that start a large-coordinate float3 (candidate position copies), with current x,y,z."""
    out_a, out_v = [], []
    mem = open(f"/proc/{pid}/mem", "rb", 0)
    for start, end in scan.regions(pid):
        off = start
        while off < end:
            n = min(64 * 1024 * 1024, end - off); n -= n % 4
            if n <= 0: break
            try: mem.seek(off); buf = mem.read(n)
            except: break
            if not buf: break
            got = len(buf) - (len(buf) % 4)
            a = np.frombuffer(buf, dtype=np.float32, count=got // 4)
            if a.size < 3: off += got; continue
            x, y, z = a[:-2], a[1:-1], a[2:]
            m = (np.abs(x) > 300) & (np.abs(y) > 300) & (np.abs(z) > 300) & \
                np.isfinite(x) & np.isfinite(y) & np.isfinite(z) & (np.abs(x) < 1e7)
            idx = np.nonzero(m)[0]
            if idx.size:
                out_a.append(off + idx.astype(np.uint64) * 4)
                out_v.append(np.stack([x[idx], y[idx], z[idx]], axis=1))
            off += got
    mem.close()
    return (np.concatenate(out_a), np.concatenate(out_v)) if out_a else (np.empty(0,np.uint64), np.empty((0,3),np.float32))

pid = int(sys.argv[2])
if sys.argv[1] == "A":
    a, v = all_pos_addrs(pid)
    np.savez("A.npz", addr=a, val=v)
    print(f"snapshot A: {a.size} candidate position floats")
else:
    d = np.load("A.npz"); a0, v0 = d["addr"], d["val"]
    a1, v1all = all_pos_addrs(pid)                 # full re-scan (fast), then intersect by address
    common, i0, i1 = np.intersect1d(a0, a1, return_indices=True)
    v0, v1 = v0[i0], v1all[i1]
    a0 = common
    delta = v1 - v0
    good = np.all(np.isfinite(delta), axis=1) & (np.linalg.norm(delta, axis=1) > 1.0)
    # dominant exact delta vector = the player's displacement (shared by all true position copies)
    keys = [tuple(np.round(delta[i], 0)) for i in np.nonzero(good)[0]]
    if not keys:
        print("no moved candidates - did the character move?"); sys.exit(1)
    dom, cnt = collections.Counter(keys).most_common(1)[0]
    print(f"dominant movement vector {dom}  shared by {cnt} copies")
    sel = good & np.all(np.round(delta, 0) == np.array(dom), axis=1)
    addr = a0[sel]
    np.savez("clean.npz", addr=addr)
    print(f"true synchronized position copies: {addr.size} -> clean.npz")
    for i in np.nonzero(sel)[0][:30]:
        print(f"  0x{int(a0[i]):x}: ({v1[i,0]:.2f},{v1[i,1]:.2f},{v1[i,2]:.2f})  d={tuple(np.round(delta[i],1))}")
