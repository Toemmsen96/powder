"""Discover the server mods in a Powder repo's ``mods/`` folder.

A mod is either a single ``<name>.lua`` file or a ``<name>/`` folder with a ``mod.lua`` inside (the same rule
the server uses). Names starting with ``_`` are examples/templates and are skipped. A mod that also ships a
non-empty ``<name>/client/`` folder carries a client mod for the player's game.
"""
from __future__ import annotations

import tomllib
from dataclasses import dataclass
from pathlib import Path


@dataclass
class ModInfo:
    name: str
    has_client: bool   # True if <repo>/mods/<name>/client/ exists with >= 1 file
    description: str    # from <name>/mod.toml `description`, else the first comment line of the lua, else ""


def _first_comment(lua: Path) -> str:
    """The text of the first ``--`` comment line in a Lua file, trimmed; "" if none readable."""
    try:
        with open(lua, encoding="utf-8", errors="replace") as fh:
            for raw in fh:
                line = raw.strip()
                if line.startswith("--"):
                    return line.lstrip("-").strip()
                if line:                              # first non-comment, non-blank line: no leading comment
                    return ""
    except OSError:
        pass
    return ""


def _has_client(folder: Path) -> bool:
    client = folder / "client"
    if not client.is_dir():
        return False
    return any(p.is_file() for p in client.rglob("*"))


def _description(folder: Path, lua: Path) -> str:
    toml = folder / "mod.toml"
    if toml.is_file():
        try:
            with open(toml, "rb") as fh:
                desc = tomllib.load(fh).get("description")
            if isinstance(desc, str) and desc.strip():
                return desc.strip()
        except (OSError, tomllib.TOMLDecodeError):
            pass
    return _first_comment(lua)


def list_server_mods(repo: str) -> list[ModInfo]:
    mods_dir = Path(repo) / "mods"
    found: dict[str, ModInfo] = {}
    if not mods_dir.is_dir():
        return []
    for entry in mods_dir.iterdir():
        if entry.name.startswith("_") or entry.name.startswith("."):
            continue
        if entry.is_file() and entry.suffix == ".lua":
            name = entry.stem
            found.setdefault(name, ModInfo(name=name, has_client=False,
                                           description=_first_comment(entry)))
        elif entry.is_dir() and (entry / "mod.lua").is_file():
            name = entry.name
            found[name] = ModInfo(name=name, has_client=_has_client(entry),
                                  description=_description(entry, entry / "mod.lua"))
    return sorted(found.values(), key=lambda m: m.name)
