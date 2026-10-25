"""Client mods in the launcher: the index, path safety, hash checks, consent (approved per server CA and
content) and which DLLs get loaded (the injection itself is Windows-only and not run here)."""
import hashlib
import io
import json
import urllib.error
from pathlib import Path

import pytest

from powder.launcher import certs, mods
from powder.launcher.config import LauncherConfig
from powder.launcher.session import OK, Session
from powder.server import tls

CA, _ = tls._make_ca()                                       # the server's CA, as the Certificate step pinned it


def sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


class FakeServer:
    """GET /_powder/mods and /_powder/mods/<name>/<path>, as the mock HTTP the launcher talks to."""
    def __init__(self, files: dict[str, dict[str, bytes]], inject: dict[str, list[str]] | None = None,
                 index: dict | None = None):
        self.files, self.inject, self.index, self.gets = files, inject or {}, index, []

    def index_json(self) -> dict:
        if self.index is not None:
            return self.index
        return {"api": 1, "mods": [
            {"name": name, "version": "1.0", "description": f"{name} mod",
             "files": [{"path": p, "size": len(b), "sha256": sha(b)} for p, b in fs.items()],
             "inject": self.inject.get(name, [p for p in fs if p.endswith(".dll")])}
            for name, fs in self.files.items()]}

    def get(self, path: str, max_bytes: int) -> bytes:
        self.gets.append(path)
        if path == "/_powder/mods":
            return json.dumps(self.index_json()).encode()
        name, rel = path.removeprefix("/_powder/mods/").split("/", 1)
        try:
            return self.files[name][rel]
        except KeyError:
            raise mods.ModError(f"{path}: HTTP 404") from None


# ---- path safety -----------------------------------------------------------------------------------------
@pytest.mark.parametrize("bad", [
    "", "/etc/passwd", "\\\\server\\share\\x.dll", "C:/Windows/x.dll", "C:x.dll", "..", "../x.dll",
    "a/../../x.dll", "a/./x.dll", "a//x.dll", "a\\..\\x.dll", "bin\\x.dll", "x.dll:stream", "CON",
    "aux.dll", "nul.txt", "COM1", "x.dll.", "x.dll ", "a/\x00.dll", "a/b?.dll", "a/"])
def test_unsafe_paths_are_rejected(bad):
    with pytest.raises(mods.ModError):
        mods.safe_rel_path(bad)


@pytest.mark.parametrize("good", ["x.dll", "bin/x.dll", "data/a b/c.ini", "..x.dll", "x..dll", "console.dll"])
def test_safe_paths(good):
    assert str(mods.safe_rel_path(good)) == good


def test_target_stays_in_the_mod_folder(tmp_path):
    root = tmp_path / "mod"
    root.mkdir()
    assert mods.target(root, "bin/x.dll") == root / "bin" / "x.dll"
    outside = tmp_path / "outside"
    outside.mkdir()
    (root / "link").symlink_to(outside, target_is_directory=True)
    with pytest.raises(mods.ModError):
        mods.target(root, "link/x.dll")                     # a link inside can't lead out


# ---- the index ----------------------------------------------------------------------------------------------
def test_parse_index():
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov", "fov.ini": b"fov=90"}, "hud": {"bin/hud.dll": b"MZ hud"}})
    offered, problems = mods.parse_index(srv.index_json())
    assert problems == []
    assert [m.name for m in offered] == ["fov", "hud"]
    fov = offered[0]
    assert fov.version == "1.0" and fov.inject == ("fov.dll",)
    assert fov.files[0] == mods.ModFile("fov.dll", 6, sha(b"MZ fov"))


def test_parse_index_refuses_bad_mods_and_keeps_the_rest():
    good = {"name": "ok", "files": [{"path": "ok.dll", "size": 1, "sha256": "a" * 64}], "inject": ["ok.dll"]}
    bad = [
        {"name": "../evil", "files": good["files"]},
        {"name": "abs", "files": [{"path": "C:/Windows/evil.dll", "size": 1, "sha256": "a" * 64}]},
        {"name": "nohash", "files": [{"path": "x.dll", "size": 1, "sha256": "zz"}]},
        {"name": "nosize", "files": [{"path": "x.dll", "size": -1, "sha256": "a" * 64}]},
        {"name": "boolsize", "files": [{"path": "x.dll", "size": True, "sha256": "a" * 64}]},
        {"name": "injectother", "files": good["files"], "inject": ["C:/Windows/System32/evil.dll"]},
        {"name": "injectmissing", "files": good["files"], "inject": ["other.dll"]},
        {"name": "injectnodll", "files": [{"path": "x.exe", "size": 1, "sha256": "a" * 64}], "inject": ["x.exe"]},
        {"name": "twice", "files": [{"path": "x.dll", "size": 1, "sha256": "a" * 64},
                                    {"path": "X.DLL", "size": 1, "sha256": "a" * 64}]},
        {"name": "empty", "files": []},
        "not an object",
    ]
    offered, problems = mods.parse_index({"api": 1, "mods": [good, *bad, dict(good)]})
    assert [m.name for m in offered] == ["ok"]
    assert len(problems) == len(bad) + 1                     # + "ok" listed twice


@pytest.mark.parametrize("index", [{"api": 2, "mods": []}, {"mods": []}, {"api": 1, "mods": {}}, []])
def test_unknown_index_format(index):
    with pytest.raises(mods.ModError):
        mods.parse_index(index)


def test_fetch_index_older_server_and_garbage():
    def missing(path, n):
        raise mods.ModError(f"{path}: HTTP 404")
    assert mods.fetch_index(missing) is None
    with pytest.raises(mods.ModError):
        mods.fetch_index(lambda path, n: b"<html>")


def test_http_reads_at_most_what_is_expected(tmp_path, monkeypatch):
    ca = tmp_path / "ca.pem"
    monkeypatch.setattr(mods.ssl, "create_default_context", lambda cafile: type("C", (), {})())
    seen = {}

    def open_url(url, context, timeout):                     # the server's no-redirect opener
        seen["url"] = url
        if url.endswith("/missing"):
            raise urllib.error.HTTPError(url, 404, "no", {}, io.BytesIO(b""))
        return io.BytesIO(b"x" * 100)
    monkeypatch.setattr(mods.server, "open_url", open_url)
    http = mods.Http("192.168.1.10", 443, ca)
    assert http.get("/_powder/mods", 100) == b"x" * 100
    assert seen["url"] == "https://192.168.1.10:443/_powder/mods"
    with pytest.raises(mods.ModError, match="larger"):
        http.get("/_powder/mods", 99)
    with pytest.raises(mods.ModError, match="HTTP 404"):
        http.get("/missing", 10)


# ---- download and hash checks ---------------------------------------------------------------------------
def test_sync_downloads_checks_and_skips_what_is_there(tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov", "cfg/fov.ini": b"fov=90"}})
    (mod,), _ = mods.parse_index(srv.index_json())
    root = tmp_path / "fov"
    assert sorted(mods.sync(mod, root, srv.get)) == ["cfg/fov.ini", "fov.dll"]
    assert (root / "cfg" / "fov.ini").read_bytes() == b"fov=90"
    srv.gets.clear()
    assert mods.sync(mod, root, srv.get) == [] and srv.gets == []      # unchanged: nothing fetched
    (root / "fov.dll").write_bytes(b"MZ tampered")
    (root / "stale.dll").write_bytes(b"old")
    assert mods.sync(mod, root, srv.get) == ["fov.dll"]
    assert (root / "fov.dll").read_bytes() == b"MZ fov" and not (root / "stale.dll").exists()
    assert mods.dlls(mod, root) == [(root / "fov.dll").resolve()]


def test_sync_refuses_files_that_do_not_match(tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    (mod,), _ = mods.parse_index(srv.index_json())
    srv.files["fov"]["fov.dll"] = b"MZ evl"                  # same size, other bytes
    with pytest.raises(mods.ModError, match="sha256"):
        mods.sync(mod, tmp_path / "fov", srv.get)
    srv.files["fov"]["fov.dll"] = b"MZ longer"
    with pytest.raises(mods.ModError, match="bytes"):
        mods.sync(mod, tmp_path / "fov", srv.get)
    assert not (tmp_path / "fov" / "fov.dll").exists()


def test_dlls_checks_again_before_loading(tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    (mod,), _ = mods.parse_index(srv.index_json())
    mods.sync(mod, tmp_path / "fov", srv.get)
    (tmp_path / "fov" / "fov.dll").write_bytes(b"MZ swapped")
    with pytest.raises(mods.ModError):
        mods.dlls(mod, tmp_path / "fov")


# ---- consent and pinning ----------------------------------------------------------------------------------
def test_decide():
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}, "hud": {"hud.dll": b"MZ hud"}, "new": {"n.dll": b"n"}})
    offered, _ = mods.parse_index(srv.index_json())
    fov, hud, new = offered
    changed = dict(hud.pin(), files={"hud.dll": "0" * 64})
    pins = {"fov": dict(fov.pin(), version="0.9"), "hud": changed}
    assert set(mods.decide(offered, False, ["fov", "hud", "new"], pins).values()) == {mods.OFF}
    assert mods.decide(offered, True, ["fov", "hud", "new"], pins) == {
        "fov": mods.ACTIVE, "hud": mods.CHANGED, "new": mods.NEW}           # only the version string changed: ok
    assert mods.decide(offered, True, ["fov"], pins) == {
        "fov": mods.ACTIVE, "hud": mods.NOT_ALLOWED, "new": mods.NOT_ALLOWED}
    assert mods.decide(offered, True, ["*"], {})["fov"] == mods.NOT_ALLOWED  # no wildcards
    reordered = dict(fov.pin(), inject=[])
    assert mods.decide([fov], True, ["fov"], {"fov": reordered}) == {"fov": mods.CHANGED}
    assert "hud.dll changed" in mods.changes(changed, hud)


def test_config_client_mods(tmp_path):
    p = tmp_path / "launcher.toml"
    p.write_text('server_address = "10.0.0.2"\n')
    cfg = LauncherConfig.load(p)
    assert not cfg.client_mods_enabled and cfg.client_mods_allow == ()     # off by default
    p.write_text('[client_mods]\nenabled = true\nallow = ["fov", "hud"]\n')
    cfg = LauncherConfig.load(p)
    assert cfg.client_mods_enabled and cfg.client_mods_allow == ("fov", "hud")
    for bad in ('[client_mods]\nenable = true\n', '[client_mods]\nenabled = "yes"\n',
                '[client_mods]\nallow = "fov"\n', 'client_mods = 1\n', 'client_mods_enabled = true\n'):
        p.write_text(bad)
        with pytest.raises(ValueError):
            LauncherConfig.load(p)


class ModSession(Session):
    def __init__(self, cfg, data_dir, srv, answers=(), ca=CA):
        super().__init__(cfg, data_dir)
        self.srv, self.answers, self.asked = srv, list(answers), []
        if ca is not None:                                   # what the Certificate step leaves
            certs.save_ca(ca, self.ca_path)
            self._pin_ca(certs.sha256(ca))

    def ask(self, key, text, actions, details=""):
        self.asked.append((key, text, actions, details))
        return self.answers.pop(0)


@pytest.fixture
def play(tmp_path, monkeypatch):
    def run(srv, enabled=True, allow=("fov",), answers=(), address="10.0.0.2", ca=CA):
        cfg = LauncherConfig(server_address=address, client_mods_enabled=enabled, client_mods_allow=tuple(allow))
        monkeypatch.setattr(mods, "Http", lambda *a, **k: srv)
        s = ModSession(cfg, tmp_path, srv, answers, ca)
        assert s.check_mods() is True                        # never stops Play
        return s
    return run


def test_off_by_default_downloads_nothing(play, tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    s = play(srv, enabled=False)
    assert s.mod_plan == [] and srv.gets == ["/_powder/mods"]
    assert s.step("mods").state == OK and "offers fov" in s.step("mods").text and "off" in s.step("mods").text
    assert s.mod_pins() == {} and not (tmp_path / "client-mods" / "10.0.0.2_443" / "fov").exists()


def test_not_allowed_is_shown_not_used(play):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}, "hud": {"hud.dll": b"MZ hud"}})
    s = play(srv, answers=["Approve and load"])
    assert [m.name for m, _ in s.mod_plan] == ["fov"] and len(s.asked) == 1
    assert "/_powder/mods/hud/hud.dll" not in srv.gets
    assert "hud: offered, not in client_mods.allow" in s.step("mods").text


def test_first_use_asks_and_refusing_is_the_first_choice(play, tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov", "fov.ini": b"fov=90"}})
    s = play(srv, answers=["Play without it"])
    (key, text, actions, details), = s.asked
    assert actions == ["Play without it", "Approve and load"]
    assert "10.0.0.2:443" in text and '"fov" 1.0' in text and "2 file(s)" in text and "loads fov.dll" in text
    assert f"{sha(b'MZ fov')}           6  fov.dll" in details and sha(b"fov=90") in details
    assert s.mod_plan == [] and s.mod_pins() == {} and srv.gets == ["/_powder/mods"]
    assert "fov: not approved, not used" in s.step("mods").text


def test_first_use_pins_then_a_change_asks(play, tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    s = play(srv, answers=["Approve and load"])
    root = tmp_path / "client-mods" / "10.0.0.2_443" / "fov"
    assert s.mod_plan == [(s.mod_plan[0][0], root)] and (root / "fov.dll").read_bytes() == b"MZ fov"
    assert s.mod_pins()["fov"]["files"] == {"fov.dll": sha(b"MZ fov")}
    settings = json.loads((tmp_path / "settings.json").read_text())
    assert list(settings["client_mod_approvals"]) == [certs.sha256(CA)]          # under the server's CA
    assert play(srv).asked == []                             # approved: no more questions

    srv.files["fov"]["fov.dll"] = b"MZ fov 2"                # the host changes it
    s = play(srv, answers=["Play without it"])
    assert s.mod_plan == [] and s.asked[0][2] == ["Play without it", "Approve new version"]
    assert "fov.dll changed" in s.asked[0][1] and sha(b"MZ fov 2") in s.asked[0][3]
    assert not root.exists()                                 # not downloaded while refused (old copy gone)...
    assert s.mod_pins()["fov"]["files"] == {"fov.dll": sha(b"MZ fov")}   # ...and the pin stays

    s = play(srv, answers=["Approve new version"])
    assert [m.name for m, _ in s.mod_plan] == ["fov"] and (root / "fov.dll").read_bytes() == b"MZ fov 2"
    assert s.mod_pins()["fov"]["files"] == {"fov.dll": sha(b"MZ fov 2")}
    assert play(srv).asked == []                             # approved: no more questions


def test_approvals_are_per_server_ca_and_content(play, tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    play(srv, answers=["Approve and load"])
    # another server (its own CA) offering a mod of the same name: asks, whatever was approved elsewhere
    other_ca, _ = tls._make_ca()
    other = FakeServer({"fov": {"fov.dll": b"MZ other fov"}})
    s = play(other, address="10.0.0.3", ca=other_ca, answers=["Play without it"])
    assert len(s.asked) == 1 and s.mod_plan == []
    assert not (tmp_path / "client-mods" / "10.0.0.3_443" / "fov").exists()
    # the same files under the same name there: still asks (approved for the other server's CA only)
    s = play(FakeServer({"fov": {"fov.dll": b"MZ fov"}}), address="10.0.0.3", ca=other_ca,
             answers=["Play without it"])
    assert len(s.asked) == 1
    # another CA at the first server's address (a new server, or someone in between): asks again
    s = play(srv, ca=other_ca, answers=["Play without it"])
    assert len(s.asked) == 1 and s.mod_plan == []
    # the first server under a new address (same CA): its approval holds
    s = play(srv, address="10.0.0.9")
    assert s.asked == [] and [m.name for m, _ in s.mod_plan] == ["fov"]


def test_no_pinned_ca_no_mods(play, tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    s = play(srv, ca=None)
    assert srv.gets == [] and s.mod_plan == [] and s.step("mods").state == OK and "pinned" in s.step("mods").text


def test_bad_download_or_unreachable_server_does_not_stop_play(play, monkeypatch, tmp_path):
    srv = FakeServer({"fov": {"fov.dll": b"MZ fov"}})
    srv.index = srv.index_json()
    srv.files["fov"]["fov.dll"] = b"MZ evi"                  # what is served doesn't match the list
    s = play(srv, answers=["Approve and load"])
    assert s.mod_plan == [] and "fov: not used" in s.step("mods").text

    class Down:
        def get(self, path, n):
            raise mods.ModError("connection refused")
    s = play(Down())
    assert s.mod_plan == [] and s.step("mods").state == OK

    def no_list(path, n):
        raise mods.ModError(f"{path}: HTTP 404")
    s = play(type("Old", (), {"get": staticmethod(no_list)})())
    assert s.step("mods").text == "this server offers no client mods"


def test_unsafe_mod_is_refused_and_writes_nothing_outside(play, tmp_path):
    srv = FakeServer({}, index={"api": 1, "mods": [
        {"name": "fov", "files": [{"path": "../../../evil.dll", "size": 1, "sha256": "a" * 64}],
         "inject": ["../../../evil.dll"]}]})
    s = play(srv)
    assert s.mod_plan == [] and "refused" in s.step("mods").text
    assert srv.gets == ["/_powder/mods"] and not any(tmp_path.parent.glob("evil.dll"))


def test_inject_in_order_and_failures_do_not_stop(play):
    srv = FakeServer({"fov": {"a.dll": b"MZ a", "b.dll": b"MZ b"}, "hud": {"hud.dll": b"MZ hud"},
                      "cam": {"cam.dll": b"MZ cam"}},
                     inject={"fov": ["b.dll", "a.dll"]})
    s = play(srv, allow=("fov", "hud", "cam"), answers=["Approve and load"] * 3)
    loaded_paths = []

    def load(pid, dll: Path):
        loaded_paths.append(dll.name)
        if dll.name == "hud.dll":
            return False
        if dll.name == "cam.dll":
            raise OSError("access denied")
        return True
    loaded, failed = s.inject_mods(1234, load=load)
    assert loaded_paths == ["b.dll", "a.dll", "hud.dll", "cam.dll"]        # mod order, then inject order
    assert loaded == ["fov"] and failed == ["hud", "cam"]
    assert all(Path(p).is_absolute() for _, p in s.mod_plan)


def test_the_list_is_bounded():
    one = {"path": "x.dll", "size": 1, "sha256": "a" * 64}
    many = [dict(one, path=f"f{i}.dll") for i in range(mods.MAX_FILES + 1)]
    big = [dict(one, path=f"f{i}.dll", size=mods.MAX_FILE) for i in range(mods.MAX_MOD_BYTES // mods.MAX_FILE + 1)]
    for bad in ({"name": "many", "files": many}, {"name": "big", "files": big},
                {"name": "n" * (mods.MAX_NAME + 1), "files": [one]},
                {"name": "long", "files": [dict(one, path="d/" * 130 + "x.dll")]},
                {"name": "notalist", "files": {"x.dll": one}}):
        with pytest.raises(mods.ModError):
            mods.parse_mod(bad)
    offered, problems = mods.parse_index({"api": 1, "mods": [{"name": f"m{i}", "files": [one]}
                                                             for i in range(mods.MAX_MODS + 5)]})
    assert len(offered) == mods.MAX_MODS and "5 mods beyond" in problems[0]


def test_texts_are_cleaned_before_they_are_shown():
    mod = mods.parse_mod({"name": "fov", "version": "1.0\n‮exe.dll\x1b[2J" + "9" * 100,
                          "description": "line\r\nbreak\tand​zero width",
                          "files": [{"path": "x.dll", "size": 1, "sha256": "a" * 64}]})
    assert mod.version.startswith("1.0 exe.dll [2J9") and len(mod.version) == mods.MAX_VERSION
    assert mod.version.endswith("...") and all(c.isprintable() for c in mod.version + mod.description)
    assert mod.description == "line break and zero width"
