//! Client mods: files a mod ships for the players' games, which the launcher downloads and loads.
//!
//! A mod folder may hold a `client/` folder (served as it is) and a `mod.toml`:
//!
//! ```toml
//! version = "1.2"
//! description = "Shows the time of day in the HUD"
//! [client]
//! inject = ["hud_clock.dll"]   # DLLs the launcher injects into the game, in order (default: client/*.dll)
//! ```
//!
//! Published over HTTPS for the launcher:
//!   GET /_powder/mods                  {"api": 1, "mods": [{name, version, description, files: [{path, size,
//!                                      sha256}], inject: [path]}]}   (only mods with client files)
//!   GET /_powder/mods/<name>/<path>    one of those files
//!
//! Only listed files are served (no paths from the request reach the file system). The launcher only loads
//! client mods its player approved, pinned to these hashes: a server cannot push code to a player unasked.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const API_VERSION: i64 = 1;
pub const INDEX_PATH: &str = "/_powder/mods";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manifest {
    pub version: String,
    pub description: String,
    pub client: ClientSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSection {
    pub inject: Option<Vec<String>>,
}

impl Manifest {
    /// `<mod>/mod.toml`, or defaults when there is none.
    pub fn load(mod_dir: &Path) -> Result<Manifest> {
        let path = mod_dir.join("mod.toml");
        if !path.is_file() {
            return Ok(Manifest::default());
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClientFile {
    /// Relative to the mod's client/ folder, with "/" separators.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct ClientMod {
    pub name: String,
    pub version: String,
    pub description: String,
    pub files: Vec<ClientFile>,
    pub inject: Vec<String>,
    root: PathBuf,
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<ClientFile>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.is_file() {
            let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let rel = path.strip_prefix(root)?.components().map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>().join("/");
            out.push(ClientFile { path: rel, size: data.len() as u64, sha256: hex::encode(Sha256::digest(&data)) });
        }
    }
    Ok(())
}

/// The client part of the mod in `mod_dir`, if it has client files.
pub fn scan(name: &str, mod_dir: &Path, manifest: &Manifest) -> Result<Option<ClientMod>> {
    let root = mod_dir.join("client");
    if !root.is_dir() {
        return Ok(None);
    }
    let mut files = vec![];
    walk(&root, &root, &mut files)?;
    if files.is_empty() {
        return Ok(None);
    }
    let is_dll = |p: &str| p.to_ascii_lowercase().ends_with(".dll");
    let inject = match &manifest.client.inject {
        Some(list) => list.iter().filter(|p| {
            let ok = is_dll(p) && files.iter().any(|f| &f.path == *p);
            if !ok {
                log::warn!(target: "powder.mods", "mod {name}: inject {p:?} is not a .dll in client/, ignored");
            }
            ok
        }).cloned().collect(),
        None => files.iter().map(|f| f.path.clone()).filter(|p| is_dll(p) && !p.contains('/')).collect(),
    };
    Ok(Some(ClientMod { name: name.into(), version: manifest.version.clone(), description: manifest.description.clone(),
                        files, inject, root }))
}

pub fn index(mods: &[ClientMod]) -> Value {
    let mods: Vec<Value> = mods.iter().map(|m| json!({
        "name": m.name, "version": m.version, "description": m.description,
        "files": m.files.iter().map(|f| json!({"path": f.path, "size": f.size, "sha256": f.sha256})).collect::<Vec<_>>(),
        "inject": m.inject,
    })).collect();
    json!({"api": API_VERSION, "mods": mods})
}

/// A listed file's bytes, for `GET /_powder/mods/<name>/<path>` (`rest` = "<name>/<path>").
pub fn file(mods: &[ClientMod], rest: &str) -> Option<Vec<u8>> {
    let (name, path) = rest.split_once('/')?;
    let m = mods.iter().find(|m| m.name == name)?;
    let f = m.files.iter().find(|f| f.path == path)?;
    std::fs::read(f.path.split('/').fold(m.root.clone(), |p, part| p.join(part))).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_index_and_serve_only_listed_files() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("hud");
        std::fs::create_dir_all(m.join("client/data")).unwrap();
        std::fs::write(m.join("client/hud.dll"), b"MZ dll").unwrap();
        std::fs::write(m.join("client/data/clock.png"), b"png").unwrap();
        std::fs::write(m.join("secret.txt"), b"not for clients").unwrap();
        std::fs::write(m.join("mod.toml"), "version = \"1.2\"\ndescription = \"HUD clock\"\n").unwrap();

        let manifest = Manifest::load(&m).unwrap();
        let mods = vec![scan("hud", &m, &manifest).unwrap().unwrap()];
        let idx = index(&mods);
        assert_eq!(idx["mods"][0]["version"], "1.2");
        assert_eq!(idx["mods"][0]["inject"], json!(["hud.dll"]));
        assert_eq!(idx["mods"][0]["files"][0]["path"], "data/clock.png");
        assert_eq!(idx["mods"][0]["files"][1]["sha256"], hex::encode(Sha256::digest(b"MZ dll")));

        assert_eq!(file(&mods, "hud/data/clock.png").as_deref(), Some(&b"png"[..]));
        for bad in ["hud/../secret.txt", "hud/../mod.toml", "hud/secret.txt", "other/hud.dll", "hud", "hud/"] {
            assert_eq!(file(&mods, bad), None, "{bad}");
        }
    }

    #[test]
    fn manifest_inject_list_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let m = dir.path().join("x");
        std::fs::create_dir_all(m.join("client/sub")).unwrap();
        std::fs::write(m.join("client/sub/b.dll"), b"b").unwrap();
        std::fs::write(m.join("client/a.txt"), b"a").unwrap();
        std::fs::write(m.join("mod.toml"), "[client]\ninject = [\"sub/b.dll\", \"a.txt\", \"missing.dll\"]\n").unwrap();
        let cm = scan("x", &m, &Manifest::load(&m).unwrap()).unwrap().unwrap();
        assert_eq!(cm.inject, vec!["sub/b.dll".to_string()]);
        std::fs::write(m.join("mod.toml"), "nope = 1\n").unwrap();
        assert!(Manifest::load(&m).is_err());
        assert!(scan("y", &dir.path().join("none"), &Manifest::default()).unwrap().is_none());
    }
}
