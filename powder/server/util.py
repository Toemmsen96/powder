"""Small shared helpers."""
from __future__ import annotations

import datetime
import os


def iso_now(delta_s: float = 0) -> str:
    """UbiServices timestamp, e.g. 2026-10-04T17:23:02.892Z"""
    t = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=delta_s)
    return t.strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def write_private(path, data: bytes) -> None:
    """Write a file holding a secret (a private key): readable by its owner only (0600 on Unix), also when it
    existed with another mode (set before the secret is written). On Windows the data folder's ACLs apply."""
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_BINARY", 0), 0o600)
    with os.fdopen(fd, "wb") as f:
        if os.name == "posix":
            os.fchmod(f.fileno(), 0o600)
        f.write(data)


def make_private(path, log) -> None:
    """An existing secret file readable by group or others (written before keys were private): made 0600 (Unix;
    logged, and a failure to change it only warns)."""
    if os.name != "posix":
        return
    mode = os.stat(path).st_mode & 0o777
    if mode & 0o077:
        try:
            os.chmod(path, 0o600)
            log.info("%s was readable by others (mode %o): now 0600", path, mode)
        except OSError as e:
            log.warning("%s is readable by others (mode %o) and could not be made private: %s", path, mode, e)
