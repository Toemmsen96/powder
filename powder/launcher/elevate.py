"""Run one command elevated (UAC) and wait for it: the hosts-file change (Powder's own program, see hostsfix.py)
and the firewall rules (PowerShell by its full path, see firewall.py). Only fixed, validated arguments are
passed (the hosts step gets an IPv4 address and nothing else).

Rule: call this only after the user was told why admin rights are needed and pressed a button for it
(see HOSTS_ADMIN_WHY in session.py); a UAC prompt must never appear out of nowhere.
Known limit (see paths.py): the installed powder-cli.exe sits in a user-writable folder."""
from __future__ import annotations

import ctypes
import ipaddress
import subprocess
from ctypes import wintypes

from .. import paths

SEE_MASK_NOCLOSEPROCESS = 0x40
SW_HIDE = 0


class SHELLEXECUTEINFO(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.DWORD), ("fMask", ctypes.c_ulong), ("hwnd", wintypes.HWND),
                ("lpVerb", wintypes.LPCWSTR), ("lpFile", wintypes.LPCWSTR), ("lpParameters", wintypes.LPCWSTR),
                ("lpDirectory", wintypes.LPCWSTR), ("nShow", ctypes.c_int), ("hInstApp", wintypes.HINSTANCE),
                ("lpIDList", ctypes.c_void_p), ("lpClass", wintypes.LPCWSTR), ("hkeyClass", wintypes.HKEY),
                ("dwHotKey", wintypes.DWORD), ("hIconOrMonitor", wintypes.HANDLE), ("hProcess", wintypes.HANDLE)]


def run_elevated(cmd: list[str]) -> int:
    """Run `cmd` (program + arguments) elevated; returns its exit code, or -1 if the user declined UAC."""
    info = SHELLEXECUTEINFO()
    info.cbSize = ctypes.sizeof(info)
    info.fMask = SEE_MASK_NOCLOSEPROCESS
    info.lpVerb = "runas"
    info.lpFile = cmd[0]
    info.lpParameters = subprocess.list2cmdline(cmd[1:])
    info.nShow = SW_HIDE
    if not ctypes.windll.shell32.ShellExecuteExW(ctypes.byref(info)):
        return -1                                  # declined (ERROR_CANCELLED) or failed to start
    ctypes.windll.kernel32.WaitForSingleObject(info.hProcess, 0xFFFFFFFF)
    code = wintypes.DWORD()
    ctypes.windll.kernel32.GetExitCodeProcess(info.hProcess, ctypes.byref(code))
    ctypes.windll.kernel32.CloseHandle(info.hProcess)
    return code.value


def hosts(action: str, address: str | None = None) -> int:
    """hostsfix add (for `address`, an IPv4 address) or remove, elevated."""
    if action == "add":
        try:
            address = str(ipaddress.IPv4Address(address or ""))
        except ValueError:
            return 1
        return run_elevated(paths.self_command("hostsfix", "add", "--address", address))
    return run_elevated(paths.self_command("hostsfix", "remove"))
