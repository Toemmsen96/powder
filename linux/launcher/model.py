"""Settings model for the Linux launcher, persisted as TOML.

``Settings`` is read with :mod:`tomllib` and written by a tiny hand-rolled serializer (the stdlib has no TOML
writer and we add no third-party dependency). Only the simple types this file uses are supported: strings,
bools, ints and lists of strings, plus the ``[[servers]]`` array of tables.
"""
from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path


def _repo_root() -> str:
    """Walk up from this file to the first directory that holds ``linux/powder-wrap``; "" if none."""
    for parent in Path(__file__).resolve().parents:
        if (parent / "linux" / "powder-wrap").exists():
            return str(parent)
    return ""


def _esc(s: str) -> str:
    """A TOML basic-string literal for ``s`` (quotes, backslash and control characters escaped)."""
    out = []
    for ch in s:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\t":
            out.append("\\t")
        elif ch == "\r":
            out.append("\\r")
        elif ord(ch) < 0x20:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def _array(values: list[str]) -> str:
    return "[" + ", ".join(_esc(v) for v in values) + "]"


@dataclass
class Server:
    name: str
    join: str = ""   # "" = the local powder-wrap server; else "host#<fingerprint>" (a remote Powder server)


@dataclass
class Settings:
    repo: str                                              # powder repo root
    server_impl: str = "rust"                              # "rust" | "python"
    overlay: bool = False
    steep_dir: str = ""                                    # POWDER_STEEP_DIR override ("" = autodetect)
    game_command: list[str] = field(default_factory=list)  # the command powder-wrap wraps; [] = not set
    enabled_mods: list[str] = field(default_factory=list)  # server mod names ([] = none; "*" = all)
    servers: list[Server] = field(default_factory=list)
    selected_server: int = 0

    @staticmethod
    def default_path() -> Path:
        base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
        return Path(base) / "powder" / "linux-launcher.toml"

    @classmethod
    def load(cls, path: "Path | None" = None) -> "Settings":
        path = Path(path) if path is not None else cls.default_path()
        if not path.exists():
            return cls(repo=_repo_root())
        with open(path, "rb") as fh:
            data = tomllib.load(fh)
        servers = [Server(name=str(s.get("name", "")), join=str(s.get("join", "")))
                   for s in data.get("servers", []) if isinstance(s, dict)]
        return cls(
            repo=str(data.get("repo") or _repo_root()),
            server_impl=str(data.get("server_impl", "rust")),
            overlay=bool(data.get("overlay", False)),
            steep_dir=str(data.get("steep_dir", "")),
            game_command=[str(x) for x in data.get("game_command", [])],
            enabled_mods=[str(x) for x in data.get("enabled_mods", [])],
            servers=servers,
            selected_server=int(data.get("selected_server", 0)),
        )

    def save(self, path: "Path | None" = None) -> None:
        path = Path(path) if path is not None else self.default_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        lines = [
            f"repo = {_esc(self.repo)}",
            f"server_impl = {_esc(self.server_impl)}",
            f"overlay = {'true' if self.overlay else 'false'}",
            f"steep_dir = {_esc(self.steep_dir)}",
            f"game_command = {_array(self.game_command)}",
            f"enabled_mods = {_array(self.enabled_mods)}",
            f"selected_server = {int(self.selected_server)}",
        ]
        for s in self.servers:
            lines += ["", "[[servers]]", f"name = {_esc(s.name)}", f"join = {_esc(s.join)}"]
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
