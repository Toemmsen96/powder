r"""The only admin step: add or remove the redirect block in the hosts file.

Run elevated by elevate.py:  powder hostsfix add --address A  |  powder hostsfix remove
--address must be an IPv4 address (the launcher resolves a host name first). Never truncates: backs the file
up next to itself (hosts.powder-<time>.bak in drivers\etc, which only admins can write: a backup into a
user-writable folder could be redirected by a junction to wherever the elevated step may write; the last
KEEP_BACKUPS are kept), writes a temp file, swaps it in (retrying while another process holds the file), then
verifies the content. Lines that are not ours are kept byte for byte (the file may be in any ANSI code page or
UTF-8, with or without a BOM); only the block's ASCII lines are added or removed. Exit code 0 = done.
"""
from __future__ import annotations

import argparse
import datetime
import ipaddress
import os
import shutil
import sys
import time
from pathlib import Path

from . import dns

KEEP_BACKUPS = 10
UTF8_BOM = b"\xef\xbb\xbf"


def _decode(raw: bytes) -> tuple[bytes, str]:
    """(BOM, text): latin-1 maps every byte to one character, so lines we don't touch are written back as read."""
    if raw.startswith((b"\xff\xfe", b"\xfe\xff")):
        raise RuntimeError("the hosts file is UTF-16; add or remove the lines by hand")
    bom = UTF8_BOM if raw.startswith(UTF8_BOM) else b""
    return bom, raw[len(bom):].decode("latin-1").replace("\r\n", "\n")


def _write_verified(path: Path, data: bytes) -> None:
    tmp = path.with_name(path.name + ".powder-tmp")
    tmp.write_bytes(data)
    for attempt in range(20):
        try:
            os.replace(tmp, path)
            break
        except PermissionError:
            if attempt == 19:
                raise
            time.sleep(0.25)
    if path.read_bytes() != data:
        raise RuntimeError("hosts verification failed")


def backup(path: Path) -> Path:
    """Copy the hosts file next to itself (the older backups beyond KEEP_BACKUPS are deleted)."""
    dst = path.with_name(f"{path.name}.powder-{datetime.datetime.now():%Y%m%d-%H%M%S-%f}.bak")
    shutil.copyfile(path, dst)
    for old in sorted(path.parent.glob(f"{path.name}.powder-*.bak"))[:-KEEP_BACKUPS]:
        old.unlink(missing_ok=True)
    return dst


def apply(action: str, address: str | None, path: Path) -> Path | None:
    """Add (for `address`, an IPv4 address) or remove the block; the backup made, None if nothing changed."""
    if action == "add":
        try:
            address = str(ipaddress.IPv4Address(address or ""))
        except ValueError as e:
            raise ValueError(f"not an IPv4 address: {address!r}") from e
    raw = path.read_bytes() if path.exists() else b""
    bom, text = _decode(raw)
    base = dns.strip_block(text)
    new = base if action == "remove" else (base.rstrip("\n") + "\n\n" + dns.hosts_block(address)).lstrip("\n")
    data = bom + new.replace("\n", "\r\n").encode("latin-1")
    if data == raw:
        return None
    saved = backup(path) if path.exists() else None
    _write_verified(path, data)
    dns.flush_dns()
    return saved


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser()
    p.add_argument("action", choices=("add", "remove"))
    p.add_argument("--address")
    a = p.parse_args(argv)
    try:
        saved = apply(a.action, a.address, dns.HOSTS_PATH)
        print(f"{a.action} ok; backup {saved}")
        return 0
    except Exception as e:                       # reported back through the exit code + output
        print(f"FAILED: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
