"""This PC's addresses in the local network: what friends type in to join a server hosted here, and what a
Join address may be: an IPv4 address or a host name, optionally followed by "#" and the server CA's SHA-256
fingerprint (or its first 16+ hex digits) as the host's Powder shows it, so the launcher can compare it."""
from __future__ import annotations

import ipaddress
import re
import socket

_LABEL = r"[A-Za-z0-9_](?:[A-Za-z0-9_-]{0,61}[A-Za-z0-9_])?"            # "_": Windows PC names have it
HOSTNAME = re.compile(rf"{_LABEL}(?:\.{_LABEL})*")


def _usable(ip: str) -> bool:
    try:
        a = ipaddress.ip_address(ip)
    except ValueError:
        return False
    return a.version == 4 and not (a.is_loopback or a.is_link_local or a.is_unspecified or a.is_multicast)


def default_route_address() -> str | None:
    """The address Windows sends from towards the internet (the main network adapter). No packet is sent:
    connecting a UDP socket only picks the route."""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
            s.connect(("192.0.2.1", 9))          # documentation address: never reached
            ip = s.getsockname()[0]
    except OSError:
        return None
    return ip if _usable(ip) else None


def lan_addresses() -> list[str]:
    """IPv4 addresses of this PC (main adapter first), without loopback and self-assigned ones."""
    found = []
    first = default_route_address()
    if first:
        found.append(first)
    try:
        infos = socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET)
    except OSError:
        infos = []
    for info in infos:
        ip = info[4][0]
        if _usable(ip) and ip not in found:
            found.append(ip)
    return found


def valid_address(text: str) -> bool:
    """An IPv4 address or a host name (RFC 1123 labels, ASCII) someone could type to join."""
    if not isinstance(text, str) or text != text.strip() or not text or len(text) > 253:
        return False
    try:
        return ipaddress.ip_address(text).version == 4
    except ValueError:
        pass
    if re.fullmatch(r"[0-9.]+", text):               # looks like a (bad) IPv4 address, not a name
        return False
    return bool(HOSTNAME.fullmatch(text))


def split_join(text: str) -> tuple[str, str | None]:
    """A Join address as typed: (address, the CA fingerprint after "#" normalized to lower-case hex, or None)."""
    address, sep, fingerprint = (text or "").strip().partition("#")
    if not sep:
        return address.strip(), None
    return address.strip(), re.sub(r"[\s:]", "", fingerprint).lower()
