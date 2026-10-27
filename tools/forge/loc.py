#!/usr/bin/env python3
"""Steep's text: the LocalizationPackage_<Language> resources (type 0x6E3C9C6F, DataPC stack). List, find, get and
set strings; `set` writes the changed packages for a `_patch_03` (an asset mod's `client/forge/DataPC/`).

A package is a dictionary coder (Re-Pair like): a table of entries, each a UTF-16 code unit or a pair of two entries,
and per string a run of 1..3-byte codes naming entries. Strings are keyed by a global string id (an item's LocString
holds one, e.g. 0x4ed2 = "GREEN MOSAICS - JAPAN OUTFIT"); they are sorted by id and cut into banks of < 64 KiB. The
format, and what is confirmed, is in LOCALIZATION.md next to this file. Unchanged packages are rebuilt byte for byte
(`test`), and new text is coded with the package's own entries (a leaf is appended for a code unit it lacks).

  loc.py list <Steep> [-l English] [--json]          # every string: id, text
  loc.py find <Steep> TEXT [-l English] [-i]         # strings that contain TEXT
  loc.py get  <Steep> ID [-l all]                    # one string (in every package with -l all)
  loc.py set  <Steep> ID TEXT [ID TEXT ...] -o mods/<mod>/client/forge/DataPC [-l LANG]...
  loc.py new-id <Steep> NAME                         # the string id `set`/items.py would give NAME
  loc.py test <Steep>                                # decode + rebuild every package copy, compare

Archives are only read; `_patch_NN` past `_patch_02` (asset mods' session archives) are left out. Needs Python 3.10+
and forge.py's zstd module.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import struct
import sys
import zlib
from dataclasses import dataclass, field

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forge  # noqa: E402

LOC_PACKAGE = 0x6E3C9C6F
TEXT_CLASS = 0xD28389B5   # the coded text object inside a package
HEAD = 0x25               # the payload up to the text object's size: id, type, language, class (kept verbatim)
BANK_MAX = 0xFFFF         # a bank's data length and its string offsets are u16
# new string ids: a free id inside the UI ids' range (6,637..28,500). An id above every shipped one (0x86ef, appended to
# the last bank) crashed the game at start (2026-10-27); renaming a shipped id works, so new ids go into the gaps.
UI_ID_RANGE = (6637, 28500)


class LocError(ValueError):
    pass


@dataclass
class Package:
    """One LocalizationPackage payload, decoded down to each string's codes (`strings`: id -> code bytes)."""
    head: bytes
    b1: int = 0                                   # codes below b1 take one byte
    entries: list[tuple[int, int]] = field(default_factory=list)   # (x, y): y == 0 leaf x, else text(y) + text(x)
    strings: dict[int, bytes] = field(default_factory=dict)
    banks: list[int] = field(default_factory=list)  # first id of each bank as shipped (for `test` only)
    _memo: dict[int, tuple[int, ...]] = field(default_factory=dict, repr=False)
    _trie: dict | None = field(default=None, repr=False)

    # --- reading ------------------------------------------------------------------------------------------------
    @classmethod
    def parse(cls, payload: bytes) -> "Package":
        if len(payload) < HEAD + 4 or struct.unpack_from("<I", payload, 8)[0] != LOC_PACKAGE:
            raise LocError("not a LocalizationPackage")
        if struct.unpack_from("<I", payload, 0x21)[0] != TEXT_CLASS:
            raise LocError("no text object at 0x21")
        size = struct.unpack_from("<I", payload, HEAD)[0]
        blob = payload[HEAD + 4:]
        if len(blob) != size:
            raise LocError(f"text object: {size} bytes, payload holds {len(blob)}")
        p = cls(payload[:HEAD])
        if not size:
            return p  # an empty package (languages the game ships no text for)
        if blob[0] != 0:
            raise LocError(f"text object starts {blob[0]:#x}, expected 0")
        p.b1 = blob[1]
        (n,) = struct.unpack_from(">H", blob, 2)
        p.entries = [struct.unpack_from(">HH", blob, 4 + 4 * k) for k in range(n)]
        o = 4 + 4 * n
        (ng,) = struct.unpack_from(">H", blob, o)
        groups = [struct.unpack_from(">III", blob, o + 2 + 12 * k) for k in range(ng)]
        o += 2 + 12 * ng
        last = -1
        for base, start, table in groups:
            if table != o:
                raise LocError(f"bank {base}: table at {table:#x}, expected {o:#x}")
            (m,) = struct.unpack_from(">H", blob, table)
            recs = [struct.unpack_from(">HH", blob, table + 2 + 4 * k) for k in range(m)]
            (end,) = struct.unpack_from(">H", blob, table + 2 + 4 * m)
            o = table + 4 + 4 * m
            offs = [0] + [r[0] for r in recs] + [end]
            ids = [base] + [base + r[1] for r in recs]
            data = blob[start:start + end]
            for k, sid in enumerate(ids):
                if sid <= last:
                    raise LocError(f"string {sid}: ids not increasing")
                p.strings[sid] = data[offs[k]:offs[k + 1]]
                last = sid
            p.banks.append(base)
        if o != (groups[0][1] if groups else len(blob)):
            raise LocError("bank data does not follow the tables")
        for (_, start, table), nxt in zip(groups, groups[1:] + [(0, len(blob), 0)]):
            (m,) = struct.unpack_from(">H", blob, table)
            if start + struct.unpack_from(">H", blob, table + 2 + 4 * m)[0] != nxt[1]:
                raise LocError("banks are not contiguous")
        return p

    def _units(self, k: int) -> tuple[int, ...]:
        """The UTF-16 code units of entry k (iterative: pair chains run deep)."""
        if k in self._memo:
            return self._memo[k]
        stack = [k]
        while stack:
            e = stack[-1]
            if e in self._memo:
                stack.pop()
                continue
            if not 0 <= e < len(self.entries):
                raise LocError(f"entry {e} out of range")
            x, y = self.entries[e]
            if y == 0:
                self._memo[e] = (x,)
                stack.pop()
            elif x in self._memo and y in self._memo:
                self._memo[e] = self._memo[y] + self._memo[x]
                stack.pop()
            else:
                if len(stack) > len(self.entries):
                    raise LocError("pair entries form a cycle")
                stack += [c for c in (x, y) if c not in self._memo]
        return self._memo[k]

    def codes(self, data: bytes) -> list[int]:
        """A string's code bytes -> entry numbers."""
        out, i, b1 = [], 0, self.b1
        while i < len(data):
            c = data[i]
            if c < b1:
                v, i = c, i + 1
            elif c < 0xFF:
                if i + 1 >= len(data):
                    raise LocError("string ends inside a code")
                v, i = b1 + ((c - b1) << 8) + data[i + 1], i + 2
            else:
                if i + 2 >= len(data):
                    raise LocError("string ends inside a code")
                v, i = data[i + 1] << 8 | data[i + 2], i + 3
            out.append(v + 1)  # entry 0 (NUL) is never coded
        return out

    def text(self, sid: int) -> str:
        units = [u for e in self.codes(self.strings[sid]) for u in self._units(e)]
        return struct.pack(f">{len(units)}H", *units).decode("utf-16-be", "surrogatepass")

    # --- writing ------------------------------------------------------------------------------------------------
    def code_bytes(self, v: int) -> bytes:
        b1 = self.b1
        if v < b1:
            return bytes([v])
        if v < b1 + ((0xFF - b1) << 8):
            return bytes([b1 + ((v - b1) >> 8), (v - b1) & 0xFF])
        if v > 0xFFFF:
            raise LocError("more than 65,536 entries")
        return bytes([0xFF, v >> 8, v & 0xFF])

    def encode(self, text: str) -> bytes:
        """`text` in the fewest code bytes this package's entries allow (shortest path over the entries' texts); a
        code unit no entry holds gets a new leaf entry at the end of the table."""
        units = struct.unpack(f">{len(text.encode('utf-16-be', 'surrogatepass')) // 2}H",
                              text.encode("utf-16-be", "surrogatepass"))
        if not self.entries:
            self.entries = [(0, 0)]  # an empty package: the NUL entry, then leaves
            self.b1 = 0xFE
        leaves = {x for x, y in self.entries if y == 0}
        for u in dict.fromkeys(units):
            if u not in leaves and u:
                self.entries.append((u, 0))
                self._trie = None
        if self._trie is None:
            self._trie = {}
            for k in range(1, len(self.entries)):
                cost = len(self.code_bytes(k - 1))
                node = self._trie
                for u in self._units(k):
                    node = node.setdefault(u, {})
                if "#" not in node or node["#"][0] > cost:
                    node["#"] = (cost, k)
        n = len(units)
        best: list[tuple[int, int, int] | None] = [None] * n + [(0, 0, 0)]  # (bytes, entry, length)
        for i in range(n - 1, -1, -1):
            node, j = self._trie, i
            while j < n and units[j] in node:
                node = node[units[j]]
                j += 1
                if "#" in node and best[j] is not None:
                    c = node["#"][0] + best[j][0]
                    if best[i] is None or c < best[i][0]:
                        best[i] = (c, node["#"][1], j - i)
            if best[i] is None:
                raise LocError(f"cannot code {text!r} at {i}")
        out, i = bytearray(), 0
        while i < n:
            _, k, ln = best[i]
            out += self.code_bytes(k - 1)
            i += ln
        return bytes(out)

    def set(self, sid: int, text: str):
        if not 0 <= sid <= 0xFFFFFFFF:
            raise LocError(f"string id {sid}: not a u32")
        self.strings = dict(sorted({**self.strings, sid: self.encode(text)}.items()))

    def build(self) -> bytes:
        """The payload: the shipped layout (dictionary, bank headers, tables, data); banks filled greedily."""
        if not self.strings:
            return self.head + struct.pack("<I", 0)
        banks: list[tuple[int, list[tuple[int, int]], bytearray]] = []
        for sid, data in self.strings.items():  # sorted by id
            if banks:
                base, recs, buf = banks[-1]
                if len(buf) + len(data) <= BANK_MAX and sid - base <= 0xFFFF:
                    recs.append((len(buf), sid - base))
                    buf += data
                    continue
            if len(data) > BANK_MAX:
                raise LocError(f"string {sid}: {len(data)} bytes, more than a bank holds")
            banks.append((sid, [], bytearray(data)))
        n = len(self.entries)
        head = bytearray(struct.pack(">BBH", 0, self.b1, n))
        for x, y in self.entries:
            head += struct.pack(">HH", x, y)
        table_at = len(head) + 2 + 12 * len(banks)
        tables = bytearray()
        offs = []
        for base, recs, buf in banks:
            offs.append(table_at + len(tables))
            tables += struct.pack(">H", len(recs))
            for r in recs:
                tables += struct.pack(">HH", *r)
            tables += struct.pack(">H", len(buf))
        data_at = table_at + len(tables)
        head += struct.pack(">H", len(banks))
        for (base, _, buf), toff in zip(banks, offs):
            head += struct.pack(">III", base, data_at, toff)
            data_at += len(buf)
        blob = bytes(head + tables + b"".join(buf for _, _, buf in banks))
        return self.head + struct.pack("<I", len(blob)) + blob

    @property
    def language(self) -> int:
        return self.head[0x11]


# --- the game's packages ----------------------------------------------------------------------------------------

def stack(game: str, base: str = "DataPC") -> list[forge.Forge]:
    """The archives of a base as the game ships them, newest first (no _patch_03+)."""
    paths = [p for p in forge.patch_stack(os.path.join(game, base + ".forge")) if os.path.exists(p)
             and int((re.search(r"_patch_(\d+)\.forge$", p) or [0, 0])[1]) <= 2]
    if not paths:
        sys.exit(f"no {base}.forge in {game}")
    return [forge.Forge(p) for p in reversed(paths)]


@dataclass
class Entry:
    archive: str
    entry: forge.Entry
    resource: forge.Resource
    package: Package

    @property
    def name(self) -> str:
        return self.entry.name

    @property
    def language(self) -> str:
        return self.name.removeprefix("LocalizationPackage_")


def packages(game: str, subtitles: bool | None = False, empty: bool = False) -> list[Entry]:
    """The newest copy of each package. subtitles: False = UI text only, True = subtitles only, None = both."""
    seen, out = set(), []
    for fg in stack(game):
        for e in fg.entries:
            if e.type != LOC_PACKAGE or e.id in seen:
                continue
            seen.add(e.id)
            if subtitles is not None and e.name.endswith("_Subtitles") != subtitles:
                continue
            res = forge.parse_datafile(fg.read(e)).resources
            if len(res) != 1 or res[0].type != LOC_PACKAGE:
                raise LocError(f"{e.name}: {len(res)} resources")
            pkg = Package.parse(res[0].payload)
            if pkg.strings or empty:
                out.append(Entry(os.path.basename(fg.path), e, res[0], pkg))
    return sorted(out, key=lambda x: x.name)


def pick(entries: list[Entry], langs: list[str] | None) -> list[Entry]:
    if not langs or "all" in langs:
        return entries
    want = {x.lower() for x in langs}
    hit = [x for x in entries if x.language.lower() in want or x.name.lower() in want]
    if len(hit) != len(want):
        have = ", ".join(x.language for x in entries)
        sys.exit(f"-l {', '.join(langs)}: not all found (packages: {have})")
    return hit


def all_ids(game: str) -> set[int]:
    """Every string id of every package (UI and subtitles, all languages)."""
    return {sid for x in packages(game, subtitles=None) for sid in x.package.strings}


def new_string_id(name: str, taken: set[int]) -> int:
    """A string id for a new text keyed by `name` (e.g. an item's name): one of the unused ids inside the UI range
    (`taken` = every shipped id plus the ones already given out), picked by the name's CRC so the same name gets the
    same id each time."""
    lo, hi = UI_ID_RANGE
    free = [i for i in range(lo, hi) if i not in taken]
    if not free:
        raise LocError("no free string id left inside the UI range")
    return free[zlib.crc32(name.encode()) % len(free)]


def write_package(folder: str, x: Entry) -> str:
    """The package as a packed datafile for powder-wrap's `_patch_03` build."""
    res = forge.Resource(x.resource.id, x.resource.type, x.resource.name, x.package.build(), x.resource.flag,
                         x.resource.pre, x.resource.extra)
    data = forge.build_datafile([res])
    check = forge.parse_datafile(data).resources
    assert len(check) == 1 and check[0].payload == res.payload
    os.makedirs(folder, exist_ok=True)
    path = os.path.join(folder, f"{x.entry.id:016x}_{forge.safe_name(x.name)}.datafile")
    with open(path, "wb") as f:
        f.write(data)
    return path


def set_strings(game: str, texts: dict[int, str], out: str, langs: list[str] | None = None) -> list[str]:
    """Write every chosen UI package (default: all that hold text) with `texts` added or changed into `out`."""
    written = []
    for x in pick(packages(game), langs):
        for sid, t in texts.items():
            x.package.set(sid, t)
        for sid, t in texts.items():
            if Package.parse(x.package.build()).text(sid) != t:
                raise LocError(f"{x.name}: string {sid} does not read back")
        written.append(write_package(out, x))
    return written


# --- commands ---------------------------------------------------------------------------------------------------

def show(s: str) -> str:
    return s.replace("\\", "\\\\").replace("\n", "\\n").replace("\r", "\\r")


def default_langs(a) -> list[str]:
    return ["English"] + (["English_Subtitles"] if a.subtitles is None else [])


def cmd_list(a):
    for x in pick(packages(a.game, subtitles=a.subtitles), a.lang or default_langs(a)):
        for sid in x.package.strings:
            t = x.package.text(sid)
            if a.json:
                print(json.dumps({"package": x.language, "id": sid, "text": t}, ensure_ascii=False))
            else:
                print(f"{sid:#07x}  {show(t)}")


def cmd_find(a):
    flags = re.I if a.ignore_case else 0
    pat = re.compile(re.escape(a.text), flags)
    hits = 0
    for x in pick(packages(a.game, subtitles=a.subtitles), a.lang or default_langs(a)):
        for sid in x.package.strings:
            t = x.package.text(sid)
            if pat.search(t):
                hits += 1
                print(f"{x.language:24} {sid:#07x} ({sid})  {show(t)}")
    print(f"{hits} string(s)", file=sys.stderr)


def cmd_get(a):
    sid = int(a.id, 0)
    found = 0
    for x in pick(packages(a.game, subtitles=None), a.lang or ["English", "English_Subtitles"]):
        if sid in x.package.strings:
            found += 1
            print(f"{x.language:24} {show(x.package.text(sid))}")
    if not found:
        print(f"{sid:#x}: in none of the packages", file=sys.stderr)
        return 1


def cmd_set(a):
    if len(a.pairs) % 2:
        sys.exit("set: give ID TEXT pairs")
    texts = {int(a.pairs[k], 0): a.pairs[k + 1] for k in range(0, len(a.pairs), 2)}
    subs = {sid for x in packages(a.game, subtitles=True) for sid in x.package.strings}
    if subs & set(texts):
        sys.exit(f"set: {', '.join(map(str, sorted(subs & set(texts))))}: subtitle strings (set writes UI packages)")
    for p in set_strings(a.game, texts, a.out, a.lang):
        print(f"  {p}")
    print(f"{len(texts)} string(s) set -> {a.out}")


def cmd_new_id(a):
    sid = new_string_id(a.name, all_ids(a.game))
    print(f"{sid:#x} ({sid})")


def cmd_test(a):
    """Every copy of every package in every archive of the stack: decode every string, rebuild, compare bytes."""
    bad = total = strings = 0
    for fg in reversed(stack(a.game)):
        for e in fg.entries:
            if e.type != LOC_PACKAGE:
                continue
            total += 1
            res = forge.parse_datafile(fg.read(e)).resources[0]
            p, note = Package(b""), ""
            try:
                p = Package.parse(res.payload)
                for sid in p.strings:
                    p.text(sid)
                strings += len(p.strings)
                built = p.build()
                same = built == res.payload
                if Package.parse(built).banks != p.banks:
                    same, note = False, " (banks differ)"
            except LocError as ex:
                same, note = False, f" (error: {ex})"
            bad += not same
            if a.verbose or not same:
                print(f"{os.path.basename(fg.path):24} {e.name:56} {len(res.payload):7} bytes, "
                      f"{len(p.strings):5} strings, {len(p.banks)} bank(s): {'identical' if same else 'DIFFERS'}"
                      + note)
    print(f"{total} package copies, {strings} strings: {total - bad} rebuilt byte for byte, {bad} not")
    return 1 if bad else 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sp = ap.add_subparsers(dest="cmd", required=True)

    def common(p, subs=True):
        p.add_argument("game", help="the Steep folder")
        p.add_argument("-l", "--lang", action="append",
                       help="package, e.g. English, French, Japanese, English_Subtitles, or all (repeatable)")
        if subs:
            p.add_argument("--subtitles", action="store_const", const=None, default=False,
                           help="also the _Subtitles packages")

    p = sp.add_parser("list", help="every string of a package (default English)")
    common(p)
    p.add_argument("--json", action="store_true", help="one JSON object per line")
    p.set_defaults(fn=cmd_list)
    p = sp.add_parser("find", help="strings that contain TEXT")
    common(p)
    p.add_argument("text")
    p.add_argument("-i", "--ignore-case", action="store_true")
    p.set_defaults(fn=cmd_find)
    p = sp.add_parser("get", help="one string by id")
    common(p, subs=False)
    p.add_argument("id", help="string id (decimal or 0x...)")
    p.set_defaults(fn=cmd_get)
    p = sp.add_parser("set", help="write packages with strings added or changed (every UI package by default)")
    common(p, subs=False)
    p.add_argument("pairs", nargs="+", metavar="ID TEXT")
    p.add_argument("-o", "--out", required=True, help="e.g. mods/<mod>/client/forge/DataPC")
    p.set_defaults(fn=cmd_set)
    p = sp.add_parser("new-id", help="the string id a new text keyed by NAME gets")
    p.add_argument("game")
    p.add_argument("name")
    p.set_defaults(fn=cmd_new_id)
    p = sp.add_parser("test", help="decode and rebuild every package copy in the DataPC stack")
    p.add_argument("game")
    p.add_argument("-v", "--verbose", action="store_true")
    p.set_defaults(fn=cmd_test)
    a = ap.parse_args(argv)
    try:
        return a.fn(a) or 0
    except LocError as ex:
        sys.exit(f"loc: {ex}")


if __name__ == "__main__":
    sys.exit(main())
