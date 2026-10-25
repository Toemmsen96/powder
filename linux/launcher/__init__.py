"""Powder's Linux launcher: a small GTK4/Libadwaita front-end (and a matching CLI) for ``powder-wrap``.

The launcher stores its own settings (``linux-launcher.toml`` under ``$XDG_CONFIG_HOME/powder``), writes a
generated server ``config.toml`` with the chosen ``[mods] enabled`` list, and starts the game through the
repository's ``linux/powder-wrap`` wrapper with the matching environment (``POWDER_SERVER``, ``POWDER_OVERLAY``,
``POWDER_STEEP_DIR``, ``POWDER_CONFIG``). It is pure Python + stdlib for everything but the GUI, which needs
PyGObject (GTK 4 + Libadwaita); the model, mods and launch helpers import without GTK so they can be scripted
and tested headlessly.
"""
from __future__ import annotations

from .model import Server, Settings
from .mods import ModInfo, list_server_mods
from . import launch

__all__ = ["Server", "Settings", "ModInfo", "list_server_mods", "launch"]
