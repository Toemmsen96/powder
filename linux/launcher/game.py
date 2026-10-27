"""Find how to start Steep without any setup: the Wine prefix holding Ubisoft Connect, the Proton it runs with, and
``umu-run`` (what Lutris uses for Proton prefixes).

Lutris' game configs (``~/.local/share/lutris/games/*.yml``, older ``~/.config/lutris/games/``) are read when there
is one for Ubisoft Connect: its prefix and arguments, and the Powder settings a player put in its environment
(``POWDER_CLIENT_MODS``, ``POWDER_OVERLAY``) so they can be taken over once. Without Lutris the prefix is found by
looking for ``UbisoftConnect.exe`` under ``~/Games``. The Proton build is the one named in the prefix's ``version``
file (umu/Proton prefixes write it), looked up where Steam and Lutris keep compatibility tools - the same search
linux/powder-wrap does for its CA step.
"""
from __future__ import annotations

import os
import shutil
from dataclasses import dataclass, field
from pathlib import Path

CONNECT_EXE = "drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/UbisoftConnect.exe"
STEEP_LAUNCH = "uplay://launch/3279"   # Connect's link that starts Steep (product 3279)
PROTON_DIRS = (".local/share/Steam/compatibilitytools.d", ".steam/root/compatibilitytools.d",
               ".local/share/lutris/runners/proton", ".local/share/lutris/runners/wine")


@dataclass
class LutrisGame:
    config: Path
    name: str
    prefix: str
    exe: str
    args: str
    env: dict[str, str] = field(default_factory=dict)
    prefix_command: str = ""


@dataclass
class Detected:
    prefix: str = ""          # the Wine prefix holding Ubisoft Connect ("" = not found)
    proton: str = ""          # a Proton folder for PROTONPATH ("" = let umu pick its default)
    umu: str = ""             # the umu-run executable ("" = not installed)
    lutris: "LutrisGame | None" = None
    problems: list[str] = field(default_factory=list)

    @property
    def connect_exe(self) -> str:
        return str(Path(self.prefix) / CONNECT_EXE) if self.prefix else ""

    def command(self) -> list[str]:
        """The command powder-wrap wraps: Ubisoft Connect, opening Steep."""
        return [self.umu, self.connect_exe, STEEP_LAUNCH] if self.umu and self.prefix else []

    def env(self) -> dict[str, str]:
        # as Lutris runs it: waitforexitandrun keeps umu-run (and so the powder-wrap session) alive until every
        # process in the prefix ended - Ubisoft Connect's first exe exits at once and leaves upc.exe running, and
        # the default verb returned there, ending the session (server stopped, CA removed) under a starting Connect;
        # STORE=ubisoft makes umu apply its Ubisoft Connect fixes
        # PROTON_USE_XALIA=0: no Xalia (GE-Proton's controller accessibility helper; it only printed exceptions
        # into the console when Connect closed)
        e = {"WINEPREFIX": self.prefix, "GAMEID": "umu-default", "STORE": "ubisoft",
             "PROTON_VERB": "waitforexitandrun", "PROTON_USE_XALIA": "0"} if self.prefix else {}
        if self.proton:
            e["PROTONPATH"] = self.proton
        return e


def _yaml_sections(text: str) -> dict[str, dict]:
    """The small part of a Lutris game yml we need: top-level sections and their direct ``key: value`` pairs,
    one level of nesting below that (``system: env: {...}``). Not a YAML parser; quoted scalars are unquoted."""
    out: dict[str, dict] = {}
    section = sub = None
    for raw in text.splitlines():
        if not raw.strip() or raw.lstrip().startswith("#") or raw.lstrip().startswith("- "):
            continue
        indent = len(raw) - len(raw.lstrip(" "))
        key, _, value = raw.strip().partition(":")
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
            value = value[1:-1]
        if indent == 0:
            section, sub = key, None
            out.setdefault(section, {})
            if value:
                out[section] = {"": value}
        elif indent == 2 and section is not None:
            if value:
                out[section][key] = value
                sub = None
            else:
                sub = key
                out[section].setdefault(sub, {})
        elif indent == 4 and section is not None and sub is not None and value:
            out[section][sub][key] = value
    return out


def lutris_games() -> list[LutrisGame]:
    """Lutris games that start Ubisoft Connect, the ones opening Steep first."""
    found = []
    for d in (Path.home() / ".local/share/lutris/games", Path.home() / ".config/lutris/games"):
        for yml in sorted(d.glob("*.yml")) if d.is_dir() else ():
            try:
                y = _yaml_sections(yml.read_text(encoding="utf-8", errors="replace"))
            except OSError:
                continue
            game, system = y.get("game", {}), y.get("system", {})
            exe = game.get("exe", "")
            if "UbisoftConnect.exe" not in exe:
                continue
            env = system.get("env", {})
            found.append(LutrisGame(config=yml, name=y.get("name", {}).get("", yml.stem), prefix=game.get("prefix", ""),
                                    exe=exe, args=game.get("args", ""),
                                    env=env if isinstance(env, dict) else {},
                                    prefix_command=system.get("prefix_command", "")))
    return sorted(found, key=lambda g: STEEP_LAUNCH not in g.args)


def find_prefix() -> str:
    for base in (Path.home() / "Games", Path.home()):
        if not base.is_dir():
            continue
        for child in sorted(base.iterdir()):
            if (child / CONNECT_EXE).is_file():
                return str(child)
    return ""


def find_proton(prefix: str) -> str:
    """The Proton build named in ``<prefix>/version`` (e.g. GE-Proton11-11), or ""."""
    try:
        version = (Path(prefix) / "version").read_text().strip()
    except OSError:
        return ""
    if not version:
        return ""
    for d in PROTON_DIRS:
        base = Path.home() / d
        for cand in sorted(base.glob(version + "*")) if base.is_dir() else ():
            if (cand / "proton").is_file() or (cand / "files" / "bin" / "wine").is_file():
                return str(cand)
    return ""


def detect(prefix_override: str = "", proton_override: str = "") -> Detected:
    d = Detected(umu=shutil.which("umu-run") or "")
    games = lutris_games()
    d.lutris = games[0] if games else None
    d.prefix = prefix_override or (d.lutris.prefix if d.lutris else "") or find_prefix()
    d.prefix = os.path.expanduser(d.prefix)
    if not d.prefix:
        d.problems.append("no Wine prefix with Ubisoft Connect found (install it with Lutris, or pick the prefix)")
    elif not Path(d.connect_exe).is_file():
        d.problems.append(f"no UbisoftConnect.exe in {d.prefix}")
    d.proton = proton_override or (find_proton(d.prefix) if d.prefix else "")
    if not d.umu:
        d.problems.append("umu-run is not installed (Arch: umu-launcher); it runs Proton outside Steam")
    return d
