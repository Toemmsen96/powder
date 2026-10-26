# Steep save files

`savegame.py` reads Steep's save files (`<Ubisoft Game Launcher>\savegames\<profile id>\3279\N.save`) and moves
them between accounts. Found 2026-10-26 from the user's own saves (format notes in the script's header):

- 0x224-byte header: header size, a 32-hex save-instance id (new each game session), the UTF-16 save name
  `STEEP[n]-tm_<date>` (`STEEP[1]` progress, `STEEP[4]` options + gear loadout; 51/54 are their mirrors).
- The body is XOR'd with a **16-byte key per account**, aligned to the end of the body. That key is the only account
  binding (no account id appears in the content), which is why copying a save to another account failed before.
- The plain body is a list of sections (hex-speak ids: `0xcafe00`, `0xc0ffeeee`, `0x11face11` = gear, ...), each an
  AnvilNext LZO block like the `.forge` datafiles (`tools/forge/forge.py`).

```
python3 savegame.py info <save>...                    # sections, key, instance id
python3 savegame.py rekey <src.save> <a save of the target account> <out.save>
```

An account's key is recovered from any one of its saves, so moving a save from account A to B needs one save of B.
Back up both accounts' save folders (and the server's `data/cloudsaves/<profile>/`) before replacing anything; the
game and Ubisoft Connect must be closed.
