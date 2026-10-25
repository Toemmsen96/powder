"""Failed sign-ins: after too many in a window, further tries are refused until it passes, so nobody on the
network can guess passwords at full speed. Counted per (address, email) and, more loosely, per address,
so signing in to one's own account does not reset the count for guesses at another one. An IPv6 address
counts for its /64 (one network hands out many), an IPv4 address (also IPv4-mapped IPv6) for itself.

A try is counted before the slow part (`attempt`, then the password is hashed off the event loop), so
concurrent guesses can't all pass the check first; one that worked takes its count back (`succeeded`).
Old entries are swept every SWEEP_EVERY_S, and at most MAX_KEYS are kept (the stalest go first).
"""
from __future__ import annotations

import ipaddress
import time
from collections import deque

SWEEP_EVERY_S = 60.0        # how often entries whose failures all left the window are dropped
MAX_KEYS = 100_000          # one per address and per (address, email) with failures in the window


def address_key(address: str) -> str:
    """What failures are counted under: an IPv6 address's /64, else the address as given."""
    try:
        ip = ipaddress.ip_address(address)
    except ValueError:
        return address
    if ip.version == 6:
        if ip.ipv4_mapped:
            return str(ip.ipv4_mapped)
        words = [int.from_bytes(ip.packed[i:i + 2], "big") for i in range(0, 8, 2)]
        return ":".join(f"{w:x}" for w in words) + "::/64"
    return address


class FailureLimiter:
    def __init__(self, max_failures: int = 10, max_per_address: int = 30, window_s: float = 300.0,
                 clock=time.monotonic):
        self.max_failures, self.max_per_address, self.window_s = max_failures, max_per_address, window_s
        self.max_keys = MAX_KEYS
        self.clock = clock
        self._failures: dict[tuple, deque] = {}
        self._last_sweep = clock()

    @staticmethod
    def _keys(address: str, email: str) -> tuple[tuple, tuple]:
        address = address_key(address)
        return (address, email.lower()), (address,)

    def _count(self, key: tuple, now: float) -> int:
        q = self._failures.get(key)
        if q is None:
            return 0
        while q and now - q[0] > self.window_s:
            q.popleft()
        if not q:
            del self._failures[key]
            return 0
        return len(q)

    def _blocked(self, keys: tuple[tuple, tuple], now: float) -> bool:
        pair, addr = keys
        return self._count(pair, now) >= self.max_failures or self._count(addr, now) >= self.max_per_address

    def _sweep(self, now: float) -> None:
        """Every SWEEP_EVERY_S, or when there are too many keys: drop the expired ones, then the stalest
        beyond max_keys."""
        if now - self._last_sweep < SWEEP_EVERY_S and len(self._failures) <= self.max_keys:
            return
        self._last_sweep = now
        for key in list(self._failures):
            self._count(key, now)
        excess = len(self._failures) - self.max_keys
        if excess > 0:
            for key in sorted(self._failures, key=lambda k: self._failures[k][-1])[:excess]:
                del self._failures[key]

    def blocked(self, address: str, email: str = "") -> bool:
        now = self.clock()
        self._sweep(now)
        return self._blocked(self._keys(address, email), now)

    def attempt(self, address: str, email: str = "") -> bool:
        """Before checking a password: False (nothing counted) while blocked, else the try counts as a failure
        until `succeeded` takes it back. Checked and counted in one step (no await between them)."""
        now = self.clock()
        keys = self._keys(address, email)
        if self._blocked(keys, now):
            return False
        for key in keys:
            self._failures.setdefault(key, deque()).append(now)
        self._sweep(now)
        return True

    def succeeded(self, address: str, email: str = "") -> None:
        """The try worked: the (address, email) count starts over, and the try is taken back from the
        address's."""
        pair, addr = self._keys(address, email)
        self._failures.pop(pair, None)
        q = self._failures.get(addr)
        if q:
            q.pop()
            if not q:
                del self._failures[addr]
