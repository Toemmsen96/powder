# Trainer / RE tools (Linux, external memory scan)

External reverse-engineering helpers for the game running under Wine/Proton, used to find in-memory values
(e.g. the player world position) by reading `/proc/<pid>/mem`. Run with **system `python3`** (needs `numpy`).
These are the tools that found Steep's authoritative player-position write (2026-10-25); the in-game overlay
(`native/overlay/`) then consumes the addresses (Trainer tab) and the access-finder takes over for pinning.

Find the game pid: `pgrep -f -i steep.exe`. `/proc/<pid>/mem` is readable (and writable) as the same user.

## Proven workflow for a world-space float3 (player position)

The position is a large-coordinate float3 the game replicates in **thousands** of copies (render, camera,
physics, per-frame scratch). The trick is isolating the **authoritative** copy (writing it moves the player;
writing a copy does nothing — it gets overwritten next frame).

1. **Locate the value** — one snapshot finds the most-replicated large float3:
   ```
   python3 findpos.py <pid> copies.npz
   ```
   (Or the slower general scanner `scan.py snapshot/narrow` with move→`changed`, still→`unchanged` cycles.)

2. **Isolate the true synchronized copies** by movement-delta correlation (robust against arrays/junk): the
   real position copies all move by the *same exact vector* when the player moves.
   ```
   python3 correlate.py A <pid>        # stand still
   # move the character, then:
   python3 correlate.py B <pid>        # -> clean.npz (copies sharing the exact displacement)
   ```

3. **Bisect to the one authoritative address** with gentle single writes + held-out observers as the oracle
   (no sustained freezing — that corrupts/wedges the game):
   ```
   python3 findauth2.py <pid> clean.npz 60 0.3   # -> prints AUTHORITATIVE: 0x....
   ```
   Verify by writing only that address (+N on X) and watching the player jump.

Addresses are **per-launch**. Feed X/Y/Z (`addr`, `+4`, `+8`) into the overlay's Trainer tab for fly/teleport,
then use the overlay's **access-finder** (page-guard + VEH) to recover the writing instruction and the struct
base pointer — the basis for a stable pointer path so it needn't be re-hunted each launch.

## Safety
- **Never sustain-freeze a large/dirty candidate set** — writing garbage to thousands of addresses every frame
  wedges the game. Use single writes (bisect) and only ever write to confirmed position copies.
- Test on your own game only.
