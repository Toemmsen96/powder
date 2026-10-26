# Find the third-person camera's position: points near the player that ORBIT it when the camera turns while the
# player stands still (the camera looks at the rider, so its position alone gives the view direction).
#   python3 findcam.py <pid> snap <name>       a snapshot of every float3 within 30 m of the player
#   python3 findcam.py <pid> orbit A B [C...]  candidates that moved between snapshots, at a ~constant distance
#                                             1.5-15 m from the (unmoved) player; with a 3x3 orthonormal matrix
#                                             before them = a transform (camera world matrix: rows, then position)
# Take A standing still, turn the camera (don't move), take B; turn again, take C. Read-only on the game.
import os, struct, sys
import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scan  # noqa: E402
from watch import modules  # noqa: E402
from stackwatch import player_position  # noqa: E402

RADIUS = 30.0


def snapshot(pid: int, centre: np.ndarray):
    addrs, vals = [], []
    mem = open(f"/proc/{pid}/mem", "rb", 0)
    for start, end in scan.regions(pid):
        off = start
        while off < end:
            n = min(64 << 20, end - off) & ~3
            if n <= 0:
                break
            try:
                mem.seek(off); buf = mem.read(n)
            except (OSError, ValueError):
                break
            if len(buf) < 12:
                break
            a = np.frombuffer(buf, dtype=np.float32, count=len(buf) // 4)
            x, y, z = a[:-2], a[1:-1], a[2:]
            m = (np.abs(x - centre[0]) < RADIUS) & (np.abs(y - centre[1]) < RADIUS) & (np.abs(z - centre[2]) < RADIUS)
            idx = np.nonzero(m)[0]
            if idx.size:
                addrs.append(off + idx.astype(np.uint64) * 4)
                vals.append(np.stack([x[idx], y[idx], z[idx]], axis=1))
            off += len(buf)
    return np.concatenate(addrs), np.concatenate(vals)


def read_floats(mem, addr, n):
    try:
        mem.seek(addr); b = mem.read(4 * n)
        return struct.unpack(f"<{n}f", b) if len(b) == 4 * n else None
    except (OSError, ValueError, OverflowError):
        return None


def main():
    pid, cmd = int(sys.argv[1]), sys.argv[2]
    base = next(lo for lo, _hi, p in modules(pid) if os.path.basename(p).lower() == "steep.exe")
    pos_addr = player_position(pid, base)
    mem = open(f"/proc/{pid}/mem", "rb", 0)
    player = np.array(read_floats(mem, pos_addr, 3), dtype=np.float32)
    if cmd == "snap":
        a, v = snapshot(pid, player)
        np.savez(f"cam_{sys.argv[3]}.npz", addr=a, val=v, player=player)
        print(f"snapshot {sys.argv[3]}: {a.size} float3 within {RADIUS} m of the player at {player}")
        return
    names = sys.argv[3:]
    snaps = [np.load(f"cam_{n}.npz") for n in names]
    moved = np.linalg.norm(snaps[-1]["player"] - snaps[0]["player"])
    print(f"player moved {moved:.2f} m between {names[0]} and {names[-1]} (should be ~0)")
    common = snaps[0]["addr"]
    for s in snaps[1:]:
        common = np.intersect1d(common, s["addr"])
    vals = []
    for s in snaps:
        idx = np.searchsorted(s["addr"], common)
        vals.append(s["val"][idx].astype(np.float64) - s["player"].astype(np.float64))   # relative to the player
    dist = np.stack([np.linalg.norm(v, axis=1) for v in vals])          # snapshots x candidates
    keep = (dist.min(0) > 1.5) & (dist.max(0) < 15) & (dist.max(0) - dist.min(0) < 0.3)
    for i in range(1, len(vals)):
        keep &= np.linalg.norm(vals[i] - vals[i - 1], axis=1) > 0.5        # it moved each time
    cand = common[keep]
    print(f"{common.size} common addresses, {cand.size} orbit the player")
    groups = {}
    for a in cand:
        groups.setdefault(int(a) >> 12, []).append(int(a))
    shown = 0
    for page, members in sorted(groups.items(), key=lambda kv: -len(kv[1])):
        for a in members[:4]:
            rows = read_floats(mem, a - 48, 12)
            ortho = ""
            if rows:
                m = np.array(rows, dtype=np.float64).reshape(3, 4)[:, :3]
                if np.allclose(m @ m.T, np.eye(3), atol=0.02):
                    ortho = "  <- 3x3 orthonormal before it: a transform matrix"
            now = read_floats(mem, a, 3)
            d = np.linalg.norm(np.array(now) - player) if now else 0
            print(f"  {a:#x}  now {tuple(round(x, 2) for x in now) if now else '?'}  dist {d:.2f}{ortho}")
            shown += 1
        if shown > 40:
            break


if __name__ == "__main__":
    main()
