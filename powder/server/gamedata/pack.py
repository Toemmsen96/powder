"""The game-data pack on disk.

  gamedata/
    MANIFEST.json            pack version, game build, sha256 of every file
    http.json                static game HTTPS replies (fixtures format)
    offers.json              the store catalogue: {"offers": [...]}
    actions.json             Ubisoft Club action definitions (no player state)
    rmc.json                 static NEX replies (fixtures-rmc format)
    connect/http.json        static Ubisoft Connect HTTPS replies (fixtures format)
    connect/demux.json       {"patch_info": b64, "products": [b64 product entry, ...], "init_fields": {...}}

A pack may come from someone else: check() (install, zip, the launcher) and the server's loading apply the
builder's allow-lists again and refuse a pack that fails (validate.py). A zip is unpacked only within
MAX_PACK_MEMBERS files and MAX_PACK_BYTES uncompressed; a folder with symbolic links is refused.
"""
from __future__ import annotations

import hashlib
import json
import shutil
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

PACK_FORMAT = 1
MAX_PACK_MEMBERS = 64                      # a pack has 7 files (plus folders)
MAX_PACK_BYTES = 512 << 20                 # uncompressed (real packs: some MB, mostly the achievement icons)
FILES = ("http.json", "offers.json", "actions.json", "rmc.json", "connect/http.json", "connect/demux.json")


class PackError(RuntimeError):
    pass


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_manifest(root: Path, info: dict) -> dict:
    manifest = {"format": PACK_FORMAT, **info,
                "files": {name: sha256(root / name) for name in FILES if (root / name).exists()}}
    (root / "MANIFEST.json").write_text(json.dumps(manifest, indent=1), encoding="utf-8")
    return manifest


def read_manifest(root: Path) -> dict:
    path = root / "MANIFEST.json"
    if not path.exists():
        raise PackError(f"no game-data pack at {root} (MANIFEST.json missing); build one with "
                        "'powder-server gamedata build <recordings> <folder>'")
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if manifest.get("format") != PACK_FORMAT:
        raise PackError(f"game-data pack format {manifest.get('format')} not supported (need {PACK_FORMAT})")
    return manifest


def verify_files(root: Path, manifest: dict) -> list[str]:
    """Names of files whose content no longer matches the manifest."""
    return [name for name, digest in manifest.get("files", {}).items()
            if not (root / name).exists() or sha256(root / name) != digest]


def check(root: Path) -> dict:
    """The manifest of a complete, unchanged pack at `root` whose contents pass validate.py; PackError says what
    is wrong otherwise."""
    from .validate import validated
    manifest = read_manifest(root)
    missing = [name for name in FILES if name not in manifest.get("files", {})]
    bad = verify_files(root, manifest)
    if missing or bad:
        raise PackError(f"the game-data pack at {root} is incomplete or changed: {', '.join(missing + bad)}")
    validated(root)
    return manifest


def zip_pack(root: Path, out: Path) -> dict:
    """Pack a checked pack into one .zip (to bring it to another PC), after the privacy check that needs no
    recordings (privacy.self_check: nothing that looks like an account's data)."""
    from . import privacy
    manifest = check(root)
    findings = privacy.self_check(root)
    if findings:
        raise PackError(f"the game-data pack at {root} looks like it holds personal data, not zipped: "
                        + "; ".join(findings[:10]))
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        for name in ("MANIFEST.json", *manifest["files"]):
            z.write(root / name, f"gamedata/{name}")
    return manifest


def _pack_root(folder: Path) -> Path:
    """The folder holding MANIFEST.json: `folder` itself or its single subfolder (a zipped folder). One with
    symbolic links is refused (copying would follow them)."""
    root = None
    if (folder / "MANIFEST.json").exists():
        root = folder
    else:
        subs = [p for p in folder.iterdir() if p.is_dir()]
        if len(subs) == 1 and (subs[0] / "MANIFEST.json").exists():
            root = subs[0]
    if root is None:
        raise PackError("this is not a game-data pack (no MANIFEST.json inside)")
    if any(p.is_symlink() for p in [root, *root.rglob("*")]):
        raise PackError("this game-data pack contains symbolic links; not installed")
    return root


def install(source: Path, target: Path) -> dict:
    """Install a pack from a .zip or a folder into `target`, replacing what is there only if the new
    pack is complete. Returns its manifest."""
    if not source.exists():
        raise PackError(f"{source} does not exist")
    staging = target.with_name(target.name + ".importing")
    shutil.rmtree(staging, ignore_errors=True)
    try:
        if source.is_dir():
            shutil.copytree(_pack_root(source), staging)
        else:
            try:
                with zipfile.ZipFile(source) as z:
                    infos = z.infolist()
                    for info in infos:                          # refuse paths that leave the folder
                        p = Path(info.filename)
                        if p.is_absolute() or ".." in p.parts or p.drive:
                            raise PackError(f"unsafe path in the zip: {info.filename}")
                    total = sum(i.file_size for i in infos)     # zipfile never inflates beyond these sizes
                    if len(infos) > MAX_PACK_MEMBERS or total > MAX_PACK_BYTES:
                        raise PackError(f"{source.name} is not a game-data pack: {len(infos)} files, {total} "
                                        f"bytes unpacked (at most {MAX_PACK_MEMBERS} files, {MAX_PACK_BYTES} bytes)")
                    z.extractall(staging)
            except zipfile.BadZipFile:
                raise PackError(f"{source.name} is not a .zip file") from None
            root = _pack_root(staging)
            if root != staging:                                 # zipped folder: lift its content up
                lifted = staging.with_name(staging.name + "2")
                shutil.rmtree(lifted, ignore_errors=True)
                root.rename(lifted)
                shutil.rmtree(staging)
                lifted.rename(staging)
        manifest = check(staging)
        old = target.with_name(target.name + ".old")
        shutil.rmtree(old, ignore_errors=True)
        if target.exists():
            target.rename(old)
        staging.rename(target)
        shutil.rmtree(old, ignore_errors=True)
        return manifest
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def remove(target: Path) -> None:
    """Delete an installed pack. Only a folder that is a pack (has MANIFEST.json) is deleted."""
    if not target.exists():
        return
    if not (target / "MANIFEST.json").is_file():
        raise PackError(f"{target} does not look like a game-data pack (no MANIFEST.json); not deleted")
    shutil.rmtree(target)


@dataclass
class ConnectProducts:
    """Steep's entries in Ubisoft Connect's owned-games reply: the server's own entries (builtin/products.py)
    for the products the pack lists, with cloud saves on (ownership.py switches them off unless configured).
    Nothing of the pack's product entries but their ids reaches Connect."""
    patch_info: bytes | None = None
    products: list[bytes] = field(default_factory=list)        # encoded product messages

    @classmethod
    def from_ids(cls, patch_info: bytes | None, ids: list[int], achievements_file: str | None) -> "ConnectProducts":
        from ..builtin import products as builtin_products
        if not ids:
            return cls(patch_info=patch_info)
        return cls(patch_info=patch_info, products=builtin_products.entries(
            cloud_saves=True, achievements_file=achievements_file, only=set(ids)))
