"""Windows Defender Firewall rules for playing with others on the local network.

  host:      "Powder server": Powder's server program, TCP on its HTTPS and CRL ports, UDP on its game
             server and Storm ports (one rule per protocol)
  every PC:  "Powder: Steep peer-to-peer": the game itself, UDP (the players' games connect to each other
             on ports they pick)

Rules are per program: Windows asks about a program that starts listening unless a rule names that program,
and port rules don't count. All rules allow only the local network (RemoteAddress LocalSubnet; a VPN with its
own subnet is not covered) on networks Windows treats as Private or Domain (not Public: on a public Wi-Fi the
"local network" is everyone there), and carry the group "Powder", so Restore system removes exactly these.
Reading rules needs no admin rights; adding and removing them does (UAC): announce it first and only call
add/remove after the user pressed a button for it (see session.py).
Uses PowerShell's NetSecurity cmdlets, whose output is not translated (netsh's is), by PowerShell's full path
in System32 and without profiles; the elevated script loads modules only from PowerShell's own folder
($PSHOME\Modules, admin-only), not from the user's module folders, which come first in PSModulePath.
"""
from __future__ import annotations

import json
import logging
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path

from .. import paths
from . import elevate

log = logging.getLogger("powder.launcher")

GROUP = "Powder"
SERVER_RULE = "Powder server"
GAME_RULE = "Powder: Steep peer-to-peer"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
CATEGORY_PROFILE = {"Private": "Private", "Public": "Public", "DomainAuthenticated": "Domain"}
PROFILES = "Domain, Private"            # where our rules apply (never Public)
TRUSTED_CATEGORIES = {"Private", "DomainAuthenticated"}


class FirewallError(Exception):
    """The firewall rules could not be read (PowerShell failed or timed out)."""


@dataclass(frozen=True)
class Rule:
    name: str
    action: str                 # "Allow" / "Block"
    enabled: bool
    program: str = ""           # normalized: environment variables expanded, lower case
    group: str = ""
    profile: str = "Any"        # "Any" or e.g. "Private, Public"
    protocol: str = "Any"       # "Any", "TCP", "UDP", ...
    ports: str = "Any"          # local ports: "Any" or e.g. "443 80" / "5000-5010"


@dataclass(frozen=True)
class Program:
    """A program the other PCs must reach, and the rule that allows it."""
    exe: Path
    rule: str
    protocols: tuple[str, ...]  # what it needs (an Allow rule for "Any" covers everything)
    ports: tuple[tuple[str, tuple[int, ...]], ...] = ()   # (protocol, local ports) it listens on; none: any port

    def ports_for(self, protocol: str) -> tuple[int, ...]:
        return next((p for proto, p in self.ports if proto == protocol), ())


def norm(path: str | Path) -> str:
    return os.path.normcase(os.path.expandvars(str(path)))


def _q(text: str) -> str:
    return "'" + text.replace("'", "''") + "'"


def powershell() -> str:
    return paths.system_exe(r"WindowsPowerShell\v1.0\powershell.exe")


def _ps(script: str, timeout: float = 90) -> str:
    try:
        r = subprocess.run([powershell(), "-NoProfile", "-NonInteractive", "-Command", script],
                           capture_output=True, text=True, timeout=timeout, creationflags=CREATE_NO_WINDOW)
    except (OSError, subprocess.SubprocessError) as e:
        raise FirewallError(f"PowerShell could not read the firewall rules ({e})") from e
    if r.returncode != 0:
        raise FirewallError(f"PowerShell could not read the firewall rules ({r.stderr.strip()[:200]})")
    return r.stdout


_RULE_FIELDS = ("$p = $r | Get-NetFirewallPortFilter -ErrorAction SilentlyContinue; "
                "[pscustomobject]@{ name = $r.DisplayName; action = [string]$r.Action; enabled = [string]$r.Enabled; "
                "group = $r.Group; profile = [string]$r.Profile; "
                "program = [Environment]::ExpandEnvironmentVariables([string]$a.Program).ToLowerInvariant(); "
                "protocol = [string]$p.Protocol; ports = [string]$p.LocalPort }")


def _parse(out: str) -> dict:
    try:
        data = json.loads(out) if out.strip() else {}
    except ValueError as e:
        raise FirewallError(f"unexpected PowerShell output: {out[:200]}") from e
    rules = data.get("rules") or []
    if isinstance(rules, dict):
        rules = [rules]
    cats = data.get("categories") or []
    if isinstance(cats, str):
        cats = [cats]
    return {"categories": cats,
            "rules": [Rule(d.get("name") or "", d.get("action") or "", str(d.get("enabled")) == "True",
                           os.path.normcase(d.get("program") or ""), d.get("group") or "", d.get("profile") or "Any",
                           d.get("protocol") or "Any", d.get("ports") or "Any") for d in rules]}


def ours() -> list[Rule]:
    """The rules the launcher added (group "Powder"); FirewallError if they can't be read."""
    out = _ps(f"$res = @(Get-NetFirewallRule -Group {_q(GROUP)} -ErrorAction SilentlyContinue | ForEach-Object "
              f"{{ $r = $_; $a = $r | Get-NetFirewallApplicationFilter; {_RULE_FIELDS} }}); "
              "[pscustomobject]@{ rules = $res } | ConvertTo-Json -Depth 4 -Compress")
    return _parse(out)["rules"]


def rules_for(programs: list[Program]) -> tuple[list[Rule], list[str]]:
    """Inbound rules (anyone's) that name one of these programs, and the current network categories."""
    targets = ", ".join(_q(norm(p.exe).lower()) for p in programs)
    out = _ps(f"$t = @({targets}); "
              "$cats = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue | ForEach-Object { [string]$_.NetworkCategory }); "
              "$res = @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue | Where-Object { $_.Program -and "
              "($t -contains [Environment]::ExpandEnvironmentVariables($_.Program).ToLowerInvariant()) } | ForEach-Object { "
              "$a = $_; $a | Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object { $_.Direction -eq 'Inbound' } | "
              f"ForEach-Object {{ $r = $_; {_RULE_FIELDS} }} }}); "
              "[pscustomobject]@{ categories = $cats; rules = $res } | ConvertTo-Json -Depth 4 -Compress")
    data = _parse(out)
    return data["rules"], data["categories"]


def _applies(rule: Rule, categories: list[str]) -> bool:
    """Enabled and active for the network this PC is on now (any network if that can't be told)."""
    if not rule.enabled:
        return False
    profiles = {p.strip() for p in rule.profile.split(",")}
    wanted = {CATEGORY_PROFILE.get(c, c) for c in categories}
    return "Any" in profiles or not wanted or bool(profiles & wanted)


def blocked(program: Program, rules: list[Rule], categories: list[str]) -> list[Rule]:
    return [r for r in rules if r.program == norm(program.exe).lower() and r.action == "Block"
            and _applies(r, categories)]


def port_set(text: str) -> set[int] | None:
    """Local ports of a rule ("443 80", "5000-5010"); None for any port (also for what can't be read)."""
    found: set[int] = set()
    for part in text.replace(",", " ").split():
        lo, _, hi = part.partition("-")
        if not lo.isdigit() or not (hi or lo).isdigit() or not int(lo) <= int(hi or lo) < 65536:
            return None
        found.update(range(int(lo), int(hi or lo) + 1))
    return found or None


def allowed(program: Program, rules: list[Rule], categories: list[str]) -> bool:
    """Do enabled Allow rules for this program cover every protocol (and port) it needs, here and now?"""
    mine = [r for r in rules if r.program == norm(program.exe).lower() and r.action == "Allow"
            and _applies(r, categories)]
    for protocol in program.protocols:
        covering = [r for r in mine if r.protocol in ("Any", protocol)]
        if not covering:
            return False
        opened = [port_set(r.ports) if r.protocol != "Any" else None for r in covering]
        if None in opened:
            continue                                     # a rule for every port of it
        needed = set(program.ports_for(protocol))
        if not needed or not needed <= set().union(*opened):
            return False
    return True


def add_script(programs: list[Program]) -> str:
    common = (f"-Group {_q(GROUP)} -Direction Inbound -Action Allow -RemoteAddress LocalSubnet "
              f"-Profile {_q(PROFILES)} "
              f"-Description {_q('Added by the Powder launcher for playing on the local network')}")
    parts = []
    for p in programs:
        exe = str(p.exe)
        # an older rule of ours for the same program (e.g. switched off) is replaced, not duplicated
        parts.append(f"Get-NetFirewallRule -Group {_q(GROUP)} -ErrorAction SilentlyContinue | Where-Object {{ "
                     f"$_.DisplayName -eq {_q(p.rule)} -and ($_ | Get-NetFirewallApplicationFilter).Program -eq "
                     f"{_q(exe)} }} | Remove-NetFirewallRule")
        for protocol in p.protocols:
            if protocol not in ("TCP", "UDP"):
                raise ValueError(f"protocol {protocol!r}")
            ports = sorted({int(n) for n in p.ports_for(protocol) if 0 < int(n) < 65536})
            local = f" -LocalPort {','.join(map(str, ports))}" if ports else ""
            parts.append(f"New-NetFirewallRule -DisplayName {_q(p.rule)} {common} -Program {_q(exe)} "
                         f"-Protocol {protocol}{local} | Out-Null")
    return "; ".join(parts)


def _elevated(script: str) -> int:
    # modules only from $PSHOME (System32, admin-only): the user's module folders come first in PSModulePath
    return elevate.run_elevated([powershell(), "-NoProfile", "-NonInteractive", "-Command",
                                 "$ErrorActionPreference = 'Stop'; $env:PSModulePath = Join-Path $PSHOME 'Modules'; "
                                 f"Import-Module NetSecurity; {script}; exit 0"])


def add(programs: list[Program]) -> int:
    """Add allow rules for these programs (elevated): exit code, -1 if UAC was declined."""
    return _elevated(add_script(programs))


def remove_ours() -> int:
    """Remove every rule of the group "Powder" (elevated)."""
    return _elevated(f"Remove-NetFirewallRule -Group {_q(GROUP)} -ErrorAction SilentlyContinue")
