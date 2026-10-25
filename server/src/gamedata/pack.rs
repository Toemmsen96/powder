//! The game-data pack on disk: MANIFEST.json (format, build info, sha256 of every file) plus the files listed
//! in assets.rs. It travels as one .zip (`gamedata zip` / `gamedata install`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const PACK_FORMAT: i64 = 1;
pub const FILES: &[&str] = &["http.json", "offers.json", "actions.json", "rmc.json", "connect/http.json",
                             "connect/demux.json"];

pub fn sha256(path: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(std::fs::read(path)?)))
}

pub fn write_manifest(root: &Path, info: Map<String, Value>) -> Result<Map<String, Value>> {
    let mut manifest = Map::new();
    manifest.insert("format".into(), json!(PACK_FORMAT));
    manifest.extend(info);
    let mut files = Map::new();
    for name in FILES {
        let p = root.join(name);
        if p.exists() {
            files.insert((*name).into(), json!(sha256(&p)?));
        }
    }
    manifest.insert("files".into(), Value::Object(files));
    std::fs::write(root.join("MANIFEST.json"), serde_json::to_string_pretty(&manifest)?)?;
    Ok(manifest)
}

pub fn read_manifest(root: &Path) -> Result<Map<String, Value>> {
    let path = root.join("MANIFEST.json");
    if !path.exists() {
        bail!("no game-data pack at {} (MANIFEST.json missing); build one with \
               'powder-server gamedata build <recordings> <folder>'", root.display());
    }
    let manifest: Map<String, Value> = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("{}", path.display()))?;
    if manifest.get("format").and_then(Value::as_i64) != Some(PACK_FORMAT) {
        bail!("game-data pack format {} not supported (need {PACK_FORMAT})",
              manifest.get("format").map(|v| v.to_string()).unwrap_or_else(|| "None".into()));
    }
    Ok(manifest)
}

/// Names of files whose content no longer matches the manifest.
pub fn verify_files(root: &Path, manifest: &Map<String, Value>) -> Vec<String> {
    let files = manifest.get("files").and_then(Value::as_object).cloned().unwrap_or_default();
    files.iter().filter(|(name, digest)| {
        let p = root.join(name);
        !p.exists() || sha256(&p).ok().as_deref() != digest.as_str()
    }).map(|(name, _)| name.clone()).collect()
}

/// The manifest of a complete, unchanged pack at `root`; an error says what is wrong otherwise.
pub fn check(root: &Path) -> Result<Map<String, Value>> {
    let manifest = read_manifest(root)?;
    let files = manifest.get("files").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut wrong: Vec<String> = FILES.iter().filter(|n| !files.contains_key(**n)).map(|n| n.to_string()).collect();
    wrong.extend(verify_files(root, &manifest));
    if !wrong.is_empty() {
        bail!("the game-data pack at {} is incomplete or changed: {}", root.display(), wrong.join(", "));
    }
    Ok(manifest)
}

/// Pack a checked pack into one .zip (to bring it to another PC).
pub fn zip_pack(root: &Path, out: &Path) -> Result<Map<String, Value>> {
    let manifest = check(root)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut z = zip::ZipWriter::new(std::fs::File::create(out)?);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let files = manifest.get("files").and_then(Value::as_object).cloned().unwrap_or_default();
    for name in std::iter::once("MANIFEST.json".to_string()).chain(files.keys().cloned()) {
        z.start_file(format!("gamedata/{name}"), options)?;
        z.write_all(&std::fs::read(root.join(&name))?)?;
    }
    z.finish()?;
    Ok(manifest)
}

/// The folder holding MANIFEST.json: `folder` itself or its single subfolder (a zipped folder).
fn pack_root(folder: &Path) -> Result<PathBuf> {
    if folder.join("MANIFEST.json").exists() {
        return Ok(folder.to_path_buf());
    }
    let subs: Vec<PathBuf> = std::fs::read_dir(folder)?.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.is_dir()).collect();
    if subs.len() == 1 && subs[0].join("MANIFEST.json").exists() {
        return Ok(subs[0].clone());
    }
    bail!("this is not a game-data pack (no MANIFEST.json inside)")
}

fn sibling(target: &Path, suffix: &str) -> PathBuf {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    target.with_file_name(format!("{name}{suffix}"))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

/// Install a pack from a .zip or a folder into `target`, replacing what is there only if the new pack is
/// complete. Returns its manifest.
pub fn install(source: &Path, target: &Path) -> Result<Map<String, Value>> {
    if !source.exists() {
        bail!("{} does not exist", source.display());
    }
    let staging = sibling(target, ".importing");
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| {
        if source.is_dir() {
            copy_dir(&pack_root(source)?, &staging)?;
        } else {
            let file = std::fs::File::open(source)?;
            let mut z = zip::ZipArchive::new(file)
                .map_err(|_| anyhow!("{} is not a .zip file", source.file_name().unwrap_or_default().to_string_lossy()))?;
            for i in 0..z.len() {
                let mut entry = z.by_index(i)?;
                // refuse paths that leave the folder
                let rel = entry.enclosed_name().ok_or_else(|| anyhow!("unsafe path in the zip: {}", entry.name()))?;
                let dest = staging.join(rel);
                if entry.is_dir() {
                    std::fs::create_dir_all(&dest)?;
                    continue;
                }
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut data = vec![];
                entry.read_to_end(&mut data)?;
                std::fs::write(dest, data)?;
            }
            let root = pack_root(&staging)?;
            if root != staging {
                // zipped folder: lift its content up
                let lifted = sibling(&staging, "2");
                let _ = std::fs::remove_dir_all(&lifted);
                std::fs::rename(&root, &lifted)?;
                std::fs::remove_dir_all(&staging)?;
                std::fs::rename(&lifted, &staging)?;
            }
        }
        let manifest = check(&staging)?;
        let old = sibling(target, ".old");
        let _ = std::fs::remove_dir_all(&old);
        if target.exists() {
            std::fs::rename(target, &old)?;
        }
        std::fs::rename(&staging, target)?;
        let _ = std::fs::remove_dir_all(&old);
        Ok(manifest)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Delete an installed pack. Only a folder that is a pack (has MANIFEST.json) is deleted.
pub fn remove(target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    if !target.join("MANIFEST.json").is_file() {
        bail!("{} does not look like a game-data pack (no MANIFEST.json); not deleted", target.display());
    }
    std::fs::remove_dir_all(target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_and_install_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("pack");
        std::fs::create_dir_all(pack.join("connect")).unwrap();
        for name in FILES {
            std::fs::write(pack.join(name), "{}").unwrap();
        }
        write_manifest(&pack, Map::new()).unwrap();
        let z = dir.path().join("p.zip");
        zip_pack(&pack, &z).unwrap();
        let target = dir.path().join("gamedata");
        install(&z, &target).unwrap();
        assert!(check(&target).is_ok());
        std::fs::write(target.join("offers.json"), "{\"x\": 1}").unwrap();
        assert!(check(&target).is_err());
        assert!(install(&dir.path().join("nope.zip"), &target).is_err());
        remove(&target).unwrap();
        assert!(!target.exists());
    }
}
