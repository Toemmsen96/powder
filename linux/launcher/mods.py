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


@dataclass
class ClientMod:
    """A mod with a part for the player's game: an overlay DLL (``src/*.cpp`` -> ``client/*.dll``), asset entries
    (``client/forge/<Base>/``, made by its ``generate.sh`` from the player's install) and/or a session step
    (``session.sh``, run by powder-wrap at each start)."""
    name: str
    description: str
    dll: bool            # builds or ships an overlay DLL
    assets: bool         # ships or generates asset entries
    session: bool        # has a session step
    content: bool        # a content mod (content.toml: built together with the others at game start)
    status: str          # "ready" | "build" (native/build.sh mods) | "generate" (its generate.sh)
    entries: set[str]    # "<Base>/<entry id>" of its generated asset entries (to spot two mods changing the same)

    @property
    def kinds(self) -> str:
        return ", ".join(k for k, on in (("content", self.content), ("overlay DLL", self.dll),
                                         ("assets", self.assets), ("session step", self.session)) if on)


def _newest(paths) -> float:
    return max((p.stat().st_mtime for p in paths), default=0.0)


def list_client_mods(repo: str) -> list[ClientMod]:
    mods_dir = Path(repo) / "mods"
    out = []
    for folder in sorted(mods_dir.iterdir()) if mods_dir.is_dir() else ():
        if not folder.is_dir() or folder.name[0] in "_.":
            continue
        client = folder / "client"
        srcs = list((folder / "src").glob("*.cpp")) if (folder / "src").is_dir() else []
        dlls = list(client.glob("*.dll")) if client.is_dir() else []
        forge = client / "forge"
        entries = {f"{base.name}/{e.name.split('_', 1)[0]}" for base in forge.iterdir() if base.is_dir()
                   for e in base.iterdir()} if forge.is_dir() else set()
        generate = (folder / "generate.sh").is_file()
        session = (folder / "session.sh").is_file()
        content = (folder / "content.toml").is_file()
        dll = bool(srcs or dlls)
        if not (dll or entries or generate or session or content):
            continue
        if srcs and (not dlls or _newest(srcs) > _newest(dlls)):
            status = "build"
        elif generate and content and not (folder / "assets").is_dir():
            status = "generate"
        elif generate and not content and not entries:
            status = "generate"
        else:
            status = "ready"
        out.append(ClientMod(name=folder.name, description=_description(folder, folder / "mod.lua"), dll=dll,
                             assets=bool(entries or (generate and not content)), session=session,
                             content=content, status=status, entries=entries))
    return out


def conflicts(mods: list[ClientMod], enabled: list[str]) -> list[str]:
    """Pairs of enabled mods whose asset entries overlap (the later one in the list wins, the other's change to
    those entries is lost): "a and b both change N entries of <Base>"."""
    on = [m for m in mods if m.name in enabled]
    msgs = []
    for i, a in enumerate(on):
        for b in on[i + 1:]:
            both = a.entries & b.entries
            if both:
                bases = sorted({e.split("/")[0] for e in both})
                msgs.append(f"{a.name} and {b.name} both change {len(both)} entries of {', '.join(bases)}")
    return msgs


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
