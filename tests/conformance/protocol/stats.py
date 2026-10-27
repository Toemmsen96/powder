"""PlayerStats2 stat groups and stat events (vendored from the Python server's services/stats.py and
transport/nex/handlers/stats.py).

GetStatisticGroup_V1 reply: String name (the profile id), u32 n, n x (String key, u8 type, 8-byte value);
type 1 = int64 LE, type 2 = float64 LE. PostEvents_V1 params: type map {StatisticEvent: 1}, u32 n,
n x (String name, u32 m, m x (String key, Variant value)); PostEvent_V1 has a single event, no count.
"""
from __future__ import annotations

import struct
from typing import NamedTuple

from .rmc import Reader, Writer, write_variant

STAT_INT, STAT_DOUBLE = 1, 2
LEVEL_KEY = "level_stat:6:infinite"


class StatRow(NamedTuple):
    key: str
    type: int
    value: bytes            # raw 8 bytes as on the wire


def decode_stat_group(body: bytes) -> tuple[str, list[StatRow]]:
    r = Reader(body)
    name = r.string()
    rows = [StatRow(r.string(), r.u8(), r.take(8)) for _ in range(r.u32())]
    if r.remaining:
        raise ValueError(f"{r.remaining} trailing bytes after stat group")
    return name, rows


def encode_stat_group(name: str, rows: list[StatRow]) -> bytes:
    w = Writer().string(name).u32(len(rows))
    for row in rows:
        w.string(row.key).u8(row.type).raw(row.value)
    return w.bytes()


def encode_value(value: int | float) -> tuple[int, bytes]:
    if isinstance(value, float):
        return STAT_DOUBLE, struct.pack("<d", value)
    return STAT_INT, struct.pack("<q", value)


def stat_value(row: StatRow) -> int | float:
    return struct.unpack("<d", row.value)[0] if row.type == STAT_DOUBLE else struct.unpack("<q", row.value)[0]


def encode_events(events: list[tuple[str, dict]], single: bool = False) -> bytes:
    """PostEvents / PostEvent params as the game builds them; fields {key: (variant type, value)}."""
    w = Writer().u32(1).string("StatisticEvent").u16(1)
    if not single:
        w.u32(len(events))
    for name, fields in events:
        w.string(name).u32(len(fields))
        for key, (t, v) in fields.items():
            write_variant(w.string(key), t, v)
    return w.bytes()
