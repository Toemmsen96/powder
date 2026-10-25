r"""Where Powder's files are, from a repo checkout (dev) or from the installed program (frozen build).

  user_root()    settings and everything Powder generates: config.toml, launcher.toml, data\, gamedata\
                 installed: %LOCALAPPDATA%\Powder (kept apart from the program, so updates and the
                 uninstaller never touch progress unless asked); dev: the current folder
  native_dir()   the TLS helper (hook_guard.dll, armer.exe)
  self_command() how Powder starts itself for the server and the elevated hosts-file step. The installed
                 build uses powder-cli.exe (a console program, started with its window hidden), because
                 the windowed Powder.exe has no stdout for logs and results.
  system_exe()   a Windows program (certutil, ipconfig, PowerShell) by its full path in System32, never by
                 bare name: a bare name is looked up in the program's own folder first, which is user-writable

Known limit: the per-user install puts powder-cli.exe in %LOCALAPPDATA%\Programs\Powder, which the user (and so
any program running as the user) can write. The elevated hosts-file step runs that file, so whatever replaced it
runs with admin rights once the user accepts the UAC prompt. A per-machine install (Program Files, admin-only)
would close that; see packaging/powder.iss.
"""
from __future__ import annotations

import os
import sys
from pathlib import Path, PureWindowsPath

FROZEN = bool(getattr(sys, "frozen", False))
REPO = Path(__file__).resolve().parents[1]
CLI_EXE = "powder-cli.exe"


def install_dir() -> Path:
    return Path(sys.executable).resolve().parent


def user_root() -> Path:
    if FROZEN:
        base = os.environ.get("LOCALAPPDATA") or str(Path.home() / "AppData" / "Local")
        return Path(base) / "Powder"
    return Path.cwd()


def native_dir() -> Path:
    if FROZEN:
        return Path(getattr(sys, "_MEIPASS", install_dir())) / "native"
    return REPO / "native" / "bin"


def self_command(*args: str) -> list[str]:
    if FROZEN:
        return [str(install_dir() / CLI_EXE), *args]
    return [sys.executable, "-m", "powder", *args]


def system32() -> PureWindowsPath:
    """Windows' System32 folder, from the API (%SystemRoot% comes from the environment, which the user sets);
    elsewhere (tests) the usual place."""
    if sys.platform == "win32":
        import ctypes
        buf = ctypes.create_unicode_buffer(260)
        n = ctypes.windll.kernel32.GetSystemDirectoryW(buf, len(buf))
        if 0 < n < len(buf):
            return PureWindowsPath(buf.value)
    return PureWindowsPath(r"C:\Windows\System32")


def system_exe(relative: str) -> str:
    r"""Full path of a program under System32 (e.g. "certutil.exe", r"WindowsPowerShell\v1.0\powershell.exe")."""
    return str(system32() / relative)
