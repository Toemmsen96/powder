"""Pure-logic tests for the Linux launcher (no GTK).

Run with the ``linux/`` directory on the path::

    PYTHONPATH=linux .venv/bin/python -m pytest tests/test_linux_launcher.py -q
"""
from __future__ import annotations

import sys
import tomllib
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "linux"))

from launcher import launch as launch_mod  # noqa: E402
from launcher.model import Server, Settings  # noqa: E402
from launcher.mods import list_server_mods  # noqa: E402


def test_settings_roundtrip(tmp_path):
    path = tmp_path / "linux-launcher.toml"
    s = Settings(
        repo=str(REPO),
        server_impl="rust",
        overlay=True,
        steep_dir="/games/steep",
        game_command=["UbisoftConnect.exe", "uplay://launch/3279"],
        enabled_mods=["admin", "fun"],
        servers=[Server("local", ""), Server("friend", "host#deadbeef")],
        selected_server=1,
    )
    s.save(path)
    loaded = Settings.load(path)
    assert loaded == s


def test_load_missing_returns_defaults(tmp_path):
    s = Settings.load(tmp_path / "does-not-exist.toml")
    assert s.server_impl == "python"
    assert s.overlay is False
    assert s.game_command == []
    assert s.enabled_mods == []
    # repo is auto-detected to the dir containing linux/powder-wrap
    assert (Path(s.repo) / "linux" / "powder-wrap").exists()


def test_default_path_uses_xdg(monkeypatch, tmp_path):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    assert Settings.default_path() == tmp_path / "powder" / "linux-launcher.toml"


def test_default_path_fallback_home(monkeypatch):
    monkeypatch.delenv("XDG_CONFIG_HOME", raising=False)
    p = Settings.default_path()
    assert p.name == "linux-launcher.toml"
    assert p.parent.name == "powder"


def test_list_server_mods_finds_admin_skips_example():
    mods = list_server_mods(str(REPO))
    names = [m.name for m in mods]
    assert "admin" in names
    assert "_example" not in names
    assert not any(n.startswith("_") for n in names)
    assert names == sorted(names)


def test_write_server_config_valid_toml(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    s = Settings(repo=str(REPO), enabled_mods=["admin", "fun"])
    path = launch_mod.write_server_config(s)
    assert path == tmp_path / "powder" / "linux-launcher-config.toml"
    with open(path, "rb") as fh:
        data = tomllib.load(fh)
    assert data["mods"]["enabled"] == ["admin", "fun"]


def test_write_server_config_star(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    s = Settings(repo=str(REPO), enabled_mods=["*", "admin"])
    path = launch_mod.write_server_config(s)
    with open(path, "rb") as fh:
        data = tomllib.load(fh)
    assert data["mods"]["enabled"] == ["*"]


def test_build_env(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    s = Settings(repo=str(REPO), server_impl="rust")
    env = launch_mod.build_env(s)
    assert env["POWDER_SERVER"] == "rust"
    assert "POWDER_CONFIG" in env
    assert Path(env["POWDER_CONFIG"]).exists()
    assert "POWDER_OVERLAY" not in env
    assert "POWDER_STEEP_DIR" not in env


def test_build_env_overlay_and_steep(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    s = Settings(repo=str(REPO), overlay=True, steep_dir="/games/steep")
    env = launch_mod.build_env(s)
    assert env["POWDER_OVERLAY"] == "1"
    assert env["POWDER_STEEP_DIR"] == "/games/steep"


def test_build_command_points_at_wrapper():
    s = Settings(repo=str(REPO), game_command=["game", "--arg"])
    cmd = launch_mod.build_command(s)
    assert cmd[0] == str(REPO / "linux" / "powder-wrap")
    assert cmd[1:] == ["game", "--arg"]


def test_launch_empty_command_raises():
    s = Settings(repo=str(REPO), game_command=[])
    with pytest.raises(ValueError):
        launch_mod.launch(s)
