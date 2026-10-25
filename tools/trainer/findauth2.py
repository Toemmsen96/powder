# Bisect a CLEAN set of synchronized position copies to the authoritative one, using held-out observers
# (never written) as the oracle. Gentle single writes.  python3 findauth2.py <pid> clean.npz [dx] [settle]
import sys, struct, time, numpy as np
pid=int(sys.argv[1]); cands=sorted(int(x) for x in np.load(sys.argv[2])["addr"])
DX=float(sys.argv[3]) if len(sys.argv)>3 else 50.0
SETTLE=float(sys.argv[4]) if len(sys.argv)>4 else 0.25
f=open(f"/proc/{pid}/mem","r+b",0)
def rdf(a):
    try: f.seek(a); b=f.read(4); return struct.unpack("<f",b)[0] if len(b)==4 else None
    except: return None
def wrf(a,v):
    try: f.seek(a); f.write(struct.pack("<f",v))
    except: pass
observers=cands[-150:]; pool=cands[:-150]
def read_x():
    xs=[v for a in observers if (v:=rdf(a)) is not None and -1e7<v<1e7]
    return float(np.median(xs)) if xs else None
print(f"observers hold X ~= {read_x()} (should be ~ player X); pool={len(pool)}")
while len(pool)>1:
    half=pool[:len(pool)//2]; other=pool[len(pool)//2:]
    x0=read_x()
    for a in half:
        v=rdf(a)
        if v is not None: wrf(a,v+DX)
    time.sleep(SETTLE)
    x1=read_x()
    moved = x0 is not None and x1 is not None and abs(x1-x0)>DX*0.4
    pool = half if moved else other
    print(f"  {len(pool):>5} left  x {x0:.1f}->{x1:.1f}  {'MOVED' if moved else '-'}")
    time.sleep(SETTLE)
print("AUTHORITATIVE:", [hex(a) for a in pool])
