"""Command-line entry point for the Powder Linux launcher (``python -m launcher``).

With no subcommand (or ``--gui``) it starts the GTK4/Libadwaita window. Every mutating subcommand loads the
settings, changes them and saves them back.
"""
from __future__ import annotations

import argparse
import sys

from . import launch as launch_mod
from .model import Server, Settings
from . import game
from .mods import conflicts, list_client_mods, list_server_mods


def _print_settings(s: Settings) -> None:
    print(f"repo:            {s.repo}")
    print(f"server_impl:     {s.server_impl}")
    print(f"overlay:         {'on' if s.overlay else 'off'}")
    print(f"steep_dir:       {s.steep_dir or '(autodetect)'}")
    print(f"game_command:    {s.game_command or '(not set)'}")
    print(f"enabled_mods:    {s.enabled_mods or '(none)'}")
    print(f"client_mods:     {s.client_mods or '(none)'}")
    print(f"wine_prefix:     {s.wine_prefix or '(detect)'}")
    print(f"proton:          {s.proton or '(detect)'}")
    print(f"selected_server: {s.selected_server}")
    if s.servers:
        print("servers:")
        for i, sv in enumerate(s.servers):
            mark = "*" if i == s.selected_server else " "
            print(f"  {mark} {sv.name} -> {sv.join or '(local)'}")


def cmd_show(s: Settings, _args) -> int:
    _print_settings(s)
    return 0


def cmd_list_mods(s: Settings, _args) -> int:
    mods = list_server_mods(s.repo)
    if not mods:
        print("(no server mods found)")
        return 0
    for m in mods:
        on = "on " if (m.name in s.enabled_mods or "*" in s.enabled_mods) else "off"
        tags = " [client]" if m.has_client else ""
        desc = f" - {m.description}" if m.description else ""
        print(f"[{on}] {m.name}{tags}{desc}")
    return 0


def cmd_enable_mod(s: Settings, args) -> int:
    if args.name not in s.enabled_mods:
        s.enabled_mods.append(args.name)
        s.save()
    print(f"enabled mod: {args.name}")
    return 0


def cmd_disable_mod(s: Settings, args) -> int:
    if args.name in s.enabled_mods:
        s.enabled_mods = [m for m in s.enabled_mods if m != args.name]
        s.save()
    print(f"disabled mod: {args.name}")
    return 0


def cmd_detect(s: Settings, _args) -> int:
    d = game.detect(s.wine_prefix, s.proton)
    print(f"prefix:  {d.prefix or '(not found)'}")
    print(f"connect: {d.connect_exe or '-'}")
    print(f"proton:  {d.proton or '(umu default)'}")
    print(f"umu-run: {d.umu or '(not installed)'}")
    if d.lutris:
        print(f"lutris:  {d.lutris.config} ({d.lutris.name}); its Powder env: "
              + (", ".join(f"{k}={v}" for k, v in d.lutris.env.items() if k.startswith("POWDER_")) or "none"))
    for p in d.problems:
        print(f"problem: {p}")
    return 1 if d.problems else 0


def cmd_list_client_mods(s: Settings, _args) -> int:
    mods = list_client_mods(s.repo)
    for m in mods:
        on = "on " if m.name in s.client_mods else "off"
        need = {"ready": "", "build": " (needs: native/build.sh mods)",
                "generate": f" (needs: mods/{m.name}/generate.sh)"}[m.status]
        print(f"[{on}] {m.name} [{m.kinds}]{need}" + (f" - {m.description}" if m.description else ""))
    for c in conflicts(mods, s.client_mods):
        print(f"warning: {c}")
    return 0


def cmd_enable_client_mod(s: Settings, args) -> int:
    known = {m.name for m in list_client_mods(s.repo)}
    if args.name not in known:
        print(f"no client mod named {args.name} (see list-client-mods)", file=sys.stderr)
        return 2
    if args.name not in s.client_mods:
        s.client_mods.append(args.name)
        s.save()
    print(f"enabled client mod: {args.name}")
    return 0


def cmd_disable_client_mod(s: Settings, args) -> int:
    if args.name in s.client_mods:
        s.client_mods = [m for m in s.client_mods if m != args.name]
        s.save()
    print(f"disabled client mod: {args.name}")
    return 0


def cmd_set_prefix(s: Settings, args) -> int:
    s.wine_prefix = args.path
    s.save()
    print(f"wine_prefix: {s.wine_prefix or '(detect)'}")
    return 0


def cmd_install_desktop(s: Settings, _args) -> int:
    """Put the launcher into the desktop's application menu (~/.local/share/applications), pointing at this repo."""
    import os
    from pathlib import Path
    src = Path(s.repo) / "linux" / "powder-launcher.desktop"
    exe = Path(s.repo) / "linux" / "powder-launcher"
    base = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share") / "applications"
    base.mkdir(parents=True, exist_ok=True)
    lines = [ln for ln in src.read_text(encoding="utf-8").splitlines() if not ln.startswith("#")]
    lines = [f'Exec="{exe}"' if ln.startswith("Exec=") else ln for ln in lines]
    out = base / "powder-launcher.desktop"
    out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"added to the application menu: {out}")
    return 0


def cmd_list_servers(s: Settings, _args) -> int:
    if not s.servers:
        print("(no servers; the local powder-wrap server is used)")
        return 0
    for i, sv in enumerate(s.servers):
        mark = "*" if i == s.selected_server else " "
        print(f"  {mark} {sv.name} -> {sv.join or '(local)'}")
    return 0


def cmd_add_server(s: Settings, args) -> int:
    s.servers = [sv for sv in s.servers if sv.name != args.name]
    s.servers.append(Server(name=args.name, join=args.join))
    s.save()
    print(f"added server: {args.name} -> {args.join or '(local)'}")
    return 0


def cmd_remove_server(s: Settings, args) -> int:
    before = len(s.servers)
    s.servers = [sv for sv in s.servers if sv.name != args.name]
    if s.selected_server >= len(s.servers):
        s.selected_server = max(0, len(s.servers) - 1)
    s.save()
    print(f"removed server: {args.name}" if len(s.servers) != before else f"no such server: {args.name}")
    return 0


def cmd_set_impl(s: Settings, args) -> int:
    s.server_impl = args.impl
    s.save()
    print(f"server_impl: {s.server_impl}")
    return 0


def cmd_set_overlay(s: Settings, args) -> int:
    s.overlay = args.state == "on"
    s.save()
    print(f"overlay: {'on' if s.overlay else 'off'}")
    return 0


def cmd_set_steep_dir(s: Settings, args) -> int:
    s.steep_dir = args.path
    s.save()
    print(f"steep_dir: {s.steep_dir or '(autodetect)'}")
    return 0


def cmd_set_game_command(s: Settings, args) -> int:
    s.game_command = list(args.cmd)
    s.save()
    print(f"game_command: {s.game_command}")
    return 0


def cmd_launch(s: Settings, args) -> int:
    if args.dry_run:
        env = launch_mod.build_env(s)
        for k in sorted(env):
            print(f"{k}={env[k]}")
        print(" ".join(launch_mod.build_command(s)))
        return 0
    try:
        proc = launch_mod.launch(s)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(f"launched powder-wrap (pid {proc.pid}); waiting for it to exit...")
    return proc.wait()


def cmd_gui(s: Settings, _args) -> int:
    from . import gui
    return gui.run(s)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="launcher", description="Powder Linux launcher")
    p.add_argument("--gui", action="store_true", help="start the GTK GUI (default with no subcommand)")
    p.add_argument("--self-test", action="store_true", help="construct the GUI without running it, then exit")
    sub = p.add_subparsers(dest="cmd")

    sub.add_parser("show", help="print the current settings").set_defaults(func=cmd_show)
    sub.add_parser("list-mods", help="list server mods and whether each is enabled").set_defaults(func=cmd_list_mods)

    sp = sub.add_parser("enable-mod", help="enable a server mod")
    sp.add_argument("name")
    sp.set_defaults(func=cmd_enable_mod)

    sp = sub.add_parser("disable-mod", help="disable a server mod")
    sp.add_argument("name")
    sp.set_defaults(func=cmd_disable_mod)

    sub.add_parser("list-servers", help="list configured servers").set_defaults(func=cmd_list_servers)

    sp = sub.add_parser("add-server", help="add or replace a server")
    sp.add_argument("name")
    sp.add_argument("join", nargs="?", default="", help='"" = local; else host#<fingerprint>')
    sp.set_defaults(func=cmd_add_server)

    sp = sub.add_parser("remove-server", help="remove a server by name")
    sp.add_argument("name")
    sp.set_defaults(func=cmd_remove_server)

    sp = sub.add_parser("set-impl", help="set the server implementation")
    sp.add_argument("impl", choices=["python", "rust"])
    sp.set_defaults(func=cmd_set_impl)

    sp = sub.add_parser("set-overlay", help="turn the in-game overlay on or off")
    sp.add_argument("state", choices=["on", "off"])
    sp.set_defaults(func=cmd_set_overlay)

    sp = sub.add_parser("set-steep-dir", help="set the Steep folder override")
    sp.add_argument("path")
    sp.set_defaults(func=cmd_set_steep_dir)

    sp = sub.add_parser("set-game-command", help="set the game command powder-wrap wraps")
    sp.add_argument("cmd", nargs=argparse.REMAINDER, help="the command and its arguments")
    sp.set_defaults(func=cmd_set_game_command)

    sub.add_parser("detect", help="show the detected prefix, Proton and umu-run").set_defaults(func=cmd_detect)
    sub.add_parser("list-client-mods", help="list mods for the game and their status").set_defaults(
        func=cmd_list_client_mods)
    sp = sub.add_parser("enable-client-mod", help="load a client mod in the game")
    sp.add_argument("name")
    sp.set_defaults(func=cmd_enable_client_mod)
    sp = sub.add_parser("disable-client-mod", help="stop loading a client mod")
    sp.add_argument("name")
    sp.set_defaults(func=cmd_disable_client_mod)
    sub.add_parser("install-desktop", help="add the launcher to the application menu").set_defaults(
        func=cmd_install_desktop)
    sp = sub.add_parser("set-prefix", help='set the Wine prefix ("" = detect)')
    sp.add_argument("path")
    sp.set_defaults(func=cmd_set_prefix)

    sp = sub.add_parser("launch", help="launch the game through powder-wrap")
    sp.add_argument("--dry-run", action="store_true", help="print the environment and command, start nothing")
    sp.set_defaults(func=cmd_launch)
    return p


def main(argv: "list[str] | None" = None) -> int:
    args = build_parser().parse_args(argv)
    if args.self_test:
        from . import gui
        return gui.self_test()
    settings = Settings.load()
    if launch_mod.import_lutris(settings, game.detect(settings.wine_prefix, settings.proton).lutris):
        settings.save()
    elif not settings.imported_lutris:
        settings.imported_lutris = True
        settings.save()
    if args.cmd is None or args.gui:
        return cmd_gui(settings, args)
    return args.func(settings, args)


if __name__ == "__main__":
    raise SystemExit(main())
