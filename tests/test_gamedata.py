"""A game-data pack from someone else: checked again on install, load and zip (validate.py), zip bombs, the
privacy check's gaps (short names, product field 21, zip / gzip bodies, zip_pack). Made-up packs only."""
import base64
import gzip
import io
import json
import shutil
import zipfile

import pytest

import synthetic as syn
from powder.server.builtin import products as builtin_products
from powder.server.gamedata import pack, privacy, validate
from powder.server.gamedata.build import build
from powder.server.services import Assets
from powder.server.transport.demux import pb
from powder.server.transport.demux.connection import PATCH_BASE_URL


def _copy(pack_dir, tmp_path):
    root = tmp_path / "gamedata"
    shutil.copytree(pack_dir, root)
    return root


def _edit(root, name, change):
    """Change one file of the pack and write its manifest again, as someone making a pack would."""
    path = root / name
    data = change(json.loads(path.read_text(encoding="utf-8")))
    path.write_text(json.dumps(data), encoding="utf-8")
    old = json.loads((root / "MANIFEST.json").read_text(encoding="utf-8"))
    pack.write_manifest(root, {k: v for k, v in old.items() if k not in ("format", "files")})


def _zip(root, out):
    with zipfile.ZipFile(out, "w") as z:
        for p in root.rglob("*"):
            if p.is_file():
                z.write(p, f"gamedata/{p.relative_to(root).as_posix()}")
    return out


def _entry(host, path, body: bytes, content_type="application/json", status=200):
    return {"method": "GET", "host": host, "path": path, "status": status, "content_type": content_type,
            "body_b64": base64.b64encode(body).decode()}


def _yaml(entry: bytes) -> str:
    return pb.first(pb.decode(entry), 17, b"").decode()


def test_a_built_pack_passes_and_round_trips(pack_dir, tmp_path):
    assert pack.check(pack_dir)["format"] == pack.PACK_FORMAT
    assert privacy.self_check(pack_dir) == []
    out = tmp_path / "pack.zip"
    pack.zip_pack(pack_dir, out)
    m = pack.install(out, tmp_path / "installed")
    assert m["files"] == pack.read_manifest(pack_dir)["files"]
    assert not Assets.load(tmp_path / "installed").empty


def test_the_product_configuration_is_the_servers_own(pack_dir, tmp_path):
    """The pack's product YAML (executables, working directory, registry paths) never reaches Connect: only its
    product ids are used, the configurations are builtin/products.py's."""
    root = _copy(pack_dir, tmp_path)
    evil = pb.encode([(1, syn.STEEP), (17, "version: 2.0\r\nroot:\r\n  start_game:\r\n    online:\r\n      "
                                          "executables:\r\n      - path:\r\n          relative: ..\\\\evil.exe\r\n")])
    _edit(root, "connect/demux.json", lambda d: {**d, "products": [base64.b64encode(evil).decode()]})
    assets = Assets.load(root)
    assert not assets.empty
    (entry,) = assets.products.products
    assert "evil.exe" not in _yaml(entry) and "steam_app_id: 460920" in _yaml(entry)      # 3280: the Steam entry
    assert entry == builtin_products.entries(cloud_saves=True, achievements_file=assets.achievements_file(),
                                             only={syn.STEEP})[0]
    assert assets.achievements_file() == "0123abcd.zip" and "achievements: 0123abcd.zip" in _yaml(entry)


@pytest.mark.parametrize("name, change, why", [
    ("connect/demux.json", lambda d: {**d, "products": [base64.b64encode(pb.encode([(1, 1234)])).decode()]},
     "product 1234 is not one of Steep's"),
    ("http.json", lambda d: d + [_entry(syn.UBI, f"/v2/applications/{syn.APP_ID}/configuration?x=1",
                                        b"<script>alert(1)</script>", "text/html")], "does not reply JSON"),
    ("connect/http.json", lambda d: d + [_entry("static3.cdn.ubi.com", "/orbit/app.js", b"{}")],
     "is not one of the replies a pack may hold"),
    ("http.json", lambda d: d + [_entry("account.ubisoft.com", f"/v1/spaces/{syn.SPACE_ID}/items/all", b"{}")],
     "is not one this server answers for"),
    ("http.json", lambda d: d + [{**_entry(syn.UBI, f"/v1/spaces/{syn.SPACE_ID}/items/all", b"{}"),
                                  "method": "POST"}], "is not one of the replies a pack may hold"),
    ("rmc.json", lambda d: {**d, "AccountManagementProtocol::GetName_V1": [{"params_sha1": "0", "body": ""}]},
     "is not one of the NEX replies"),
    ("actions.json", lambda d: {"actions": [{"id": "1", "profileId": syn.PLAYER}]}, "player state"),
])
def test_a_pack_outside_the_allow_lists_is_refused(pack_dir, tmp_path, name, change, why):
    root = _copy(pack_dir, tmp_path)
    _edit(root, name, change)
    with pytest.raises(pack.PackError, match=why):
        pack.check(root)
    with pytest.raises(pack.PackError, match="refused"):
        pack.install(_zip(root, tmp_path / "p.zip"), tmp_path / "installed")
    assert not (tmp_path / "installed").exists()
    assert Assets.load(root).empty, "the server runs on its built-in replies"


def test_json_is_served_as_json_and_the_patch_url_is_the_servers(pack_dir, tmp_path):
    root = _copy(pack_dir, tmp_path)
    path = f"/v1/spaces/{syn.SPACE_ID}/items/all"
    _edit(root, "http.json", lambda d: d + [_entry(syn.UBI, path, b'{"items": []}', "text/html")])
    patch = pb.encode([(1, 1), (2, "release"), (4, "https://evil.example/patches/"), (5, 13368), (9, "x")])
    _edit(root, "connect/demux.json", lambda d: {**d, "patch_info": base64.b64encode(patch).decode()})
    assets = Assets.load(root)
    fixture, _ = assets.http.lookup("GET", syn.UBI, path)
    assert fixture.content_type == "application/json"
    assert pb.decode(assets.products.patch_info) == [(1, 1, pb.VARINT), (2, b"release", pb.LEN),
                                                     (4, PATCH_BASE_URL.encode(), pb.LEN), (5, 13368, pb.VARINT)]


def test_zip_bombs_are_refused(pack_dir, tmp_path, monkeypatch):
    many = tmp_path / "many.zip"
    with zipfile.ZipFile(many, "w") as z:
        for i in range(pack.MAX_PACK_MEMBERS + 1):
            z.writestr(f"gamedata/f{i}", b"")
    with pytest.raises(pack.PackError, match="at most"):
        pack.install(many, tmp_path / "installed")
    big = tmp_path / "big.zip"
    with zipfile.ZipFile(big, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("gamedata/http.json", b"\0" * (1 << 20))
    monkeypatch.setattr(pack, "MAX_PACK_BYTES", 1 << 19)
    with pytest.raises(pack.PackError, match="bytes unpacked"):
        pack.install(big, tmp_path / "installed")
    # the achievements zip inside the pack: capped too
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("1.png", b"\0" * (1 << 20))
    monkeypatch.setattr(validate, "MAX_ZIP_BYTES", 1 << 19)
    with pytest.raises(pack.PackError, match="unpacks to"):
        validate.read_zip(buf.getvalue())
    with pytest.raises(pack.PackError, match="unexpected file"):
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as z:
            z.writestr("../evil.dll", b"MZ")
        validate.read_zip(buf.getvalue())


def test_the_achievements_zip_is_read_once(assets, monkeypatch):
    fresh = Assets(connect_http=assets.connect_http)
    calls = []
    real = validate.read_zip
    monkeypatch.setattr(validate, "read_zip", lambda data: calls.append(1) or real(data))
    assert fresh.achievement_icon(1) == syn.ICON and fresh.achievement_icon(4) == syn.ICON
    assert fresh.achievement_names()[1] == syn.ACHIEVEMENT_NAMES[1]
    assert len(calls) == 1


def test_a_folder_with_links_is_refused(pack_dir, tmp_path):
    root = _copy(pack_dir, tmp_path)
    (root / "extra").symlink_to(tmp_path)
    with pytest.raises(pack.PackError, match="symbolic links"):
        pack.install(root, tmp_path / "installed")


def test_short_names_are_found_as_words(tmp_path):
    (tmp_path / "http.json").write_text(json.dumps({"text": "Bob rides; Bobsled and SKI"}))
    markers = {privacy.Name("Bob"), privacy.Name("Ski"), privacy.Name("led"), "ab12"}
    assert privacy.check(tmp_path, markers) == ["http.json: Bob..."]
    seen = set()
    privacy._walk({"nameOnPlatform": "Zed", "userId": "abc"}, seen)
    assert seen == {"Zed"} and isinstance(next(iter(seen)), privacy.Name)


def test_inner_zip_and_gzip_bodies_are_searched(tmp_path):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("en-US_loc.txt", f"1\t{syn.FRIEND_NAME}\tx\n" * 20)
    entries = [{"body_b64": base64.b64encode(buf.getvalue()).decode()},
               {"body_b64": base64.b64encode(gzip.compress(syn.PLAYER_EMAIL.encode())).decode()}]
    (tmp_path / "http.json").write_text(json.dumps(entries))
    found = privacy.check(tmp_path, {syn.FRIEND_NAME, syn.PLAYER_EMAIL})
    assert len(found) == 2 and all("decoded, " in f for f in found)


def test_product_field_21_must_be_steeps(recordings_dir, tmp_path):
    """A 40-hex field 21 other than Steep's known one could be per account: the builder writes the known one, the
    check (also before zipping) reports any other."""
    known = privacy.known_product_hash(3279)
    assert known and privacy.HEX40.fullmatch(known)
    recordings = tmp_path / "recordings"
    shutil.copytree(recordings_dir, recordings)
    demux = json.loads((recordings / "demux.json").read_text())
    reply = pb.decode(base64.b64decode(demux["services"]["ownership_service"]["2"][0]))
    init = pb.decode(pb.first(pb.decode(pb.first(reply, 1)), 2))
    mine = b"A" * 40
    products = [pb.encode(pb.decode(v) + [(21, mine)]) for n, v, _ in pb.decode(pb.first(init, 2))]
    demux["services"]["ownership_service"]["2"] = [base64.b64encode(pb.encode([(1, [(1, 1), (2, [
        (1, 1), (2, [(1, p) for p in products]), (5, 0), (6, 0)])])])).decode()]
    (recordings / "demux.json").write_text(json.dumps(demux))
    out = tmp_path / "gamedata"
    assert not build(recordings, out).findings
    written = [pb.decode(base64.b64decode(p)) for p in json.loads((out / "connect/demux.json").read_text())["products"]]
    assert {pb.first(p, 1): pb.first(p, 21) for p in written} == {3279: known, 3280: None, 4001: None}
    assert privacy.product_hash_findings(out) == []
    _edit(out, "connect/demux.json", lambda d: {**d, "products": [base64.b64encode(products[0]).decode()]})
    (finding,) = privacy.product_hash_findings(out)
    assert "product 3279 field 21" in finding
    with pytest.raises(pack.PackError, match="personal data"):
        pack.zip_pack(out, tmp_path / "p.zip")


def test_zip_pack_refuses_a_pack_with_account_data(pack_dir, tmp_path):
    root = _copy(pack_dir, tmp_path)
    _edit(root, "offers.json", lambda d: {"offers": d["offers"] + [{"offerId": "x", "note": syn.PLAYER_EMAIL}]})
    pack.check(root)                                  # installable (someone else's data is not a danger to us)
    with pytest.raises(pack.PackError, match="personal data"):
        pack.zip_pack(root, tmp_path / "p.zip")
    assert not (tmp_path / "p.zip").exists()
