# Bisect a CLEAN set of synchronized position copies (correlate.py's clean.npz) to the authoritative one: nudge
# half the pool's X and see whether held-out observers (never written) follow.
#   python3 findauth2.py <pid> clean.npz [dx=30] [settle=0.3]
#
# The player must be MOVING (riding downhill) the whole time: a resting physics body is asleep and the game does
# not re-sync the copies. Safety (learned the hard way, 2026-10-26): it only writes addresses that hold the
# player's X right now (never memory the game has reused since the snapshot: that corrupts game state), puts
# every nudge back right after the reading (no drift into the air), and checks before each step that the player
# is still moving, waiting instead of guessing. A step counts only if exactly one half moves the player.
import sys, struct, time, random, collections
import numpy as np

pid = int(sys.argv[1])
cands = sorted(int(x) for x in np.load(sys.argv[2])["addr"])
DX = float(sys.argv[3]) if len(sys.argv) > 3 else 30.0
SETTLE = float(sys.argv[4]) if len(sys.argv) > 4 else 0.3
f = open(f"/proc/{pid}/mem", "r+b", 0)

clusters = collections.defaultdict(list)
for a in cands:
    clusters[a >> 20].append(a)
random.seed(1)
observers = set()
for c in sorted(clusters.values(), key=len, reverse=True)[:2]:     # observers: the largest (render) clusters
    observers.update(random.sample(c, min(60, len(c))))
pool = [a for a in cands if a not in observers]


def rdf(a):
    try:
        f.seek(a); b = f.read(4)
        return struct.unpack("<f", b)[0] if len(b) == 4 else None
    except OSError:
        return None


def wrf(a, v):
    try:
        f.seek(a); f.write(struct.pack("<f", v))
    except OSError:
        pass


def read_x():
    xs = [v for a in observers if (v := rdf(a)) is not None and -1e7 < v < 1e7]
    return float(np.median(xs)) if xs else None


def moving(wait=20.0) -> bool:
    end = time.time() + wait
    while time.time() < end:
        a = read_x(); time.sleep(0.15); b = read_x()
        if a is not None and b is not None and abs(b - a) > 0.02:
            return True
        print("   (player not moving - waiting)", flush=True)
        time.sleep(0.5)
    return False


def nudge(group) -> bool:
    """Does nudging this group's X move the player (the observers)? Undone right after."""
    if not moving():
        sys.exit("the player stopped for 20 s; giving up (ride, and run again)")
    x0 = read_x()
    live = [a for a in group if (v := rdf(a)) is not None and abs(v - x0) < 50]
    for a in live:
        wrf(a, rdf(a) + DX)
    time.sleep(SETTLE)
    x1 = read_x()
    for a in live:
        v = rdf(a)
        if v is not None:
            wrf(a, v - DX)
    time.sleep(SETTLE)
    return x1 is not None and abs(x1 - x0) > DX * 0.4


print(f"{len(cands)} copies, {len(observers)} observers, pool {len(pool)}", flush=True)
retries = 0
while len(pool) > 1:
    half, other = pool[:len(pool) // 2], pool[len(pool) // 2:]
    if nudge(half):
        pool = half
    elif nudge(other):
        pool = other
    else:
        retries += 1
        if retries > 4:
            sys.exit("neither half moves the player: the authoritative copy is not in the pool (stale copies?)")
        print("   neither half moved the player; retrying the step", flush=True)
        continue
    print(f"  {len(pool):>5} left", flush=True)
ok = sum(nudge(pool) for _ in range(3))
print(f"AUTHORITATIVE: {pool[0]:#x} (verified {ok}/3)" if ok >= 2 else f"candidate {pool[0]:#x} NOT verified ({ok}/3)")
