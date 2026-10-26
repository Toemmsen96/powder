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
   (no sustained freezing — that corrupts/wedges the game). **Ride downhill the whole time** (a resting body is
   asleep: the copies don't re-sync) and run it right after step 2, on the same gear (a gear change or respawn
   rebuilds the player's objects and the copies go stale):
   ```
   python3 findauth2.py <pid> clean.npz          # -> prints AUTHORITATIVE: 0x.... (verified n/3)
   ```
   Verify by writing only that address (+N on X) and watching the player jump.

Addresses are **per-launch**. Feed X/Y/Z (`addr`, `+4`, `+8`) into the overlay's Trainer tab for fly/teleport.

## Who writes it, and noclip (watch.py, skiphook.py)

Don't use the overlay's in-process access-finder on the position (its page guard crashes the game on hot pages)
or debug registers (VMProtect clears them). `watch.py` uses Linux perf hardware watchpoints from outside instead:
the kernel traps the write, the game sees nothing.

4. **Find the writers** (once; the player moving):
   ```
   python3 watch.py <pid> <pos addr> 10          # -> steep.exe+0x... writer instructions, base reg, field offset
   ```
5. **Every later launch, find the position in seconds** from a writer's RVA (stable, unlike the data):
   ```
   python3 watch.py <pid> steep.exe+0x... 3      # execute breakpoint -> the addresses it writes (moving)
   ```
6. **Noclip:** hook the writers so the game's writes into the player's position are skipped; physics/collision
   can no longer move the player, the overlay's Fly (or a poke) moves it freely:
   ```
   python3 skiphook.py <pid> install steep.exe+0x... [steep.exe+0x...]   # 5-byte jmp -> cave in section slack
   python3 skiphook.py <pid> on <pos addr>        # skip writes into [pos, pos+12)
   python3 skiphook.py <pid> off | status | remove
   ```
   Install after step 5 (a hooked writer runs in its cave, so the execute breakpoint on the site no longer fires).
   Only writes into the given range are skipped, so other bodies sharing the code are unaffected. Patching uses
   `/proc/<pid>/mem` (writes read-only code pages; VMProtect blocks only the game's own VirtualProtect).
   Needs binutils (`objdump`, `as`, `ld`, `objcopy`, `nm`) and `kernel.perf_event_paranoid <= 2`.

## Safety
- **Never write snapshot addresses blindly.** The game reuses memory quickly; writing +dx into addresses that
  held the position a minute ago corrupts unrelated state (2026-10-26: the player ended stuck floating, needed a
  restart). findauth2 only writes addresses that hold the player's X at that moment, and undoes each nudge.
- **Never sustain-freeze a large/dirty candidate set** — writing garbage to thousands of addresses every frame
  wedges the game. Use single writes (bisect) and only ever write to confirmed position copies.
- Test on your own game only.

## Reverse engineering the unpacked game (dumpexe.py, unwind.py, stackwatch.py)

Steep is VMProtect-packed on disk; its code is only readable in memory.

```
python3 dumpexe.py <pid> steep.exe steep.dump.exe     # 95 MiB PE: file offsets = RVAs, loads in disassemblers
python3 stackwatch.py <pid> steep.dump.exe --player 15 # full call stacks of every write to the player's position
```

The dump's exception directory (.pdata) is intact: ~155k functions with exact bounds and unwind info. stackwatch
copies the writing thread's user stack at each perf watchpoint hit and unwinds it offline with that unwind info
(unwind.py = RtlVirtualUnwind's rules), so it shows the real call chain into a write, not a guess. Turn the
overlay's Fly/noclip off while recording (hooked writers run in caves without unwind info).

The overlay's crash log (powder_overlay.log, "GAME CRASH ...") gives module+offset of a crash; look it up in the
dump the same way (unwind.Image(dump, base).func(rva)).
