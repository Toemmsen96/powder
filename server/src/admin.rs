//! The admin CLI's commands over a [`Services`] (`powder-server admin ...`; safe while the server runs: SQLite
//! WAL). Each writes what the Python server's `admin/cli.py` prints, line for line, so scripts and habits carry
//! over; main.rs parses the arguments and picks the player (default: the configured default player).

use crate::assets::Assets;
use crate::config::Config;
use crate::constants::CREDITS_ITEM;
use crate::db::Db;
use crate::repos::Repos;
use crate::services::importer::{import_recordings, ImportError};
use crate::services::stats::{stat_value, Stat, LEVEL_KEY, PROFILE_GROUP};
use crate::services::Services;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// Open (or create) data/powder.db, moving a database from before the rename there first, with the game-data pack.
pub fn open(config: Config) -> Result<Services> {
    let config = Arc::new(config);
    let db = Arc::new(Db::open(&Services::move_legacy_db(config.data_dir())?)?);
    db.migrate()?;
    let assets = if config.server.use_gamedata {
        Assets::load(config.gamedata_dir())
    } else {
        log::info!(target: "powder", "game-data pack switched off (use_gamedata = false): built-in replies only");
        Assets::default()
    };
    Ok(Services::new(config, Arc::new(assets), Arc::new(Repos::new(db)), None))
}

/// The player a command is about: the one given, else the default player.
pub fn player_id(s: &Services, player: Option<&str>) -> Result<String> {
    Ok(match player.filter(|p| !p.is_empty()) {
        Some(p) => p.to_lowercase(),
        None => s.accounts.default_profile_id()?.to_lowercase(),
    })
}

/// `grant [PLAYER] ITEM QUANTITY` -> (player, item, quantity). As Python's argparse + `_grant_args`: with three
/// values whose second is a number, the first is the item and the default player is meant.
pub fn grant_args(values: &[String]) -> Result<(Option<String>, String, i64)> {
    let int = |v: &str| v.trim().parse::<i64>().with_context(|| format!("invalid int value: {v:?}"));
    let digits = |v: &str| {
        let v = v.trim_start_matches('-');
        !v.is_empty() && v.chars().all(|c| c.is_ascii_digit())
    };
    match values {
        [item, quantity] => Ok((None, item.clone(), int(quantity)?)),
        [player, item, _] if digits(item) => Ok((None, player.clone(), int(item)?)),
        [player, item, quantity] => Ok((Some(player.clone()), item.clone(), int(quantity)?)),
        _ => bail!("grant takes [PLAYER] ITEM QUANTITY"),
    }
}

pub fn players(s: &Services, out: &mut dyn Write) -> Result<()> {
    for p in s.repos.players()? {
        let level = s.stats.get(&p.profile_id, LEVEL_KEY, PROFILE_GROUP)?.map(|l| l.to_string());
        writeln!(out, "{}  {:24} level {}  credits {}  since {}", p.profile_id, p.name,
                 level.as_deref().unwrap_or("None"), s.economy.credits(&p.profile_id)?, p.created_at)?;
    }
    Ok(())
}

/// `item`: a full item id or a prefix of it.
pub fn inventory(s: &Services, pid: &str, item: Option<&str>, out: &mut dyn Write) -> Result<()> {
    let rows = s.repos.inventory(pid)?;
    let Some(item) = item else {
        writeln!(out, "{} items, {} credits", rows.len(), s.economy.credits(pid)?)?;
        return Ok(());
    };
    let prefix = item.to_lowercase();
    let hits: Vec<_> = rows.iter().filter(|r| r.item_id.starts_with(&prefix)).collect();
    for r in &hits {
        writeln!(out, "{}  quantity {}  (modified {})", r.item_id, r.quantity, r.last_modified)?;
    }
    if hits.is_empty() {
        writeln!(out, "not owned")?;
    }
    Ok(())
}

/// Add (or with a negative quantity remove) an item or credits. Ok(false): refused (would go negative).
pub fn grant(s: &Services, pid: &str, item: &str, quantity: i64, out: &mut dyn Write) -> Result<bool> {
    let item = if item.to_lowercase() == "credits" { CREDITS_ITEM } else { item };
    s.accounts.player(pid)?;
    match s.economy.grant(pid, item, quantity, "admin")? {
        Ok(c) => writeln!(out, "{}: {} -> {}", c.item_id, c.before, c.after)?,
        Err(e) => {
            writeln!(out, "{}", e.message)?;
            return Ok(false);
        }
    }
    Ok(true)
}

/// `set`: "KEY=VALUE" (a float if VALUE has a '.', else an integer); `key`: show only keys containing it.
pub fn stats(s: &Services, pid: &str, key: Option<&str>, set: Option<&str>, out: &mut dyn Write) -> Result<()> {
    if let Some(set) = set {
        let (k, value) = set.split_once('=').unwrap_or((set, ""));
        let value = if value.contains('.') {
            Stat::Double(value.trim().parse().with_context(|| format!("could not convert string to float: {value:?}"))?)
        } else {
            Stat::Int(value.trim().parse().with_context(|| format!("invalid literal for int(): {value:?}"))?)
        };
        s.stats.set(pid, k, value, PROFILE_GROUP)?;
    }
    for row in s.stats.group(pid, PROFILE_GROUP)? {
        if key.is_none_or(|k| k.is_empty() || row.key.contains(k)) {
            writeln!(out, "{:48} {}", row.key, stat_value(&row))?;
        }
    }
    Ok(())
}

pub fn ledger(s: &Services, pid: &str, n: i64, out: &mut dyn Write) -> Result<()> {
    for e in s.repos.recent_ledger(pid, n)? {
        let offer = e.offer_id.filter(|o| !o.is_empty()).map(|o| format!("  offer {o}")).unwrap_or_default();
        writeln!(out, "{}  {:10} {}  {:+} -> {}{offer}", e.ts, e.reason, e.item_id, e.delta, e.balance)?;
    }
    Ok(())
}

pub fn events(s: &Services, pid: &str, n: i64, out: &mut dyn Write) -> Result<()> {
    for e in s.repos.recent_stat_events(pid, n)? {
        writeln!(out, "{}  {:32} {}", e.ts, e.name, e.fields_json)?;
    }
    Ok(())
}

/// Ubisoft Connect cloud saves: every player's (`player` None) or one's; `delete` removes the player's.
pub fn cloud_saves(s: &Services, pid: &str, player: Option<&str>, delete: bool, out: &mut dyn Write) -> Result<()> {
    if delete {
        writeln!(out, "removed {} cloud save(s) of {pid}", s.cloudsaves.delete(pid, None)?)?;
        return Ok(());
    }
    let items = s.cloudsaves.all(player.map(|_| pid))?;
    for item in &items {
        let e = &item.entity;
        let slots: Vec<String> = match e.get("obj").and_then(|o| o.get("slots")) {
            Some(Value::Array(slots)) => slots.iter().filter_map(Value::as_object)
                .map(|x| format!("{} ({} B)", py_get(x, "filename"), py_get(x, "size"))).collect(),
            _ => vec![],
        };
        writeln!(out, "{}  product {}  revision {}  {}  {}  {}", item.profile_id, item.product, py_get(e, "revision"),
                 py_get(e, "lastModified"), if item.complete { "complete" } else { "INCOMPLETE" }, slots.join(", "))?;
    }
    if items.is_empty() {
        writeln!(out, "no cloud saves")?;
    }
    Ok(())
}

/// Personal best per challenge.
pub fn scores(s: &Services, pid: &str, out: &mut dyn Write) -> Result<()> {
    for e in s.repos.scores(pid)? {
        writeln!(out, "{:48} {:>12}  ({}, {})", e.challenge_id, format_g(e.best_score), e.direction, e.updated)?;
    }
    Ok(())
}

/// Earned achievements per product, with Steep's names from the game-data pack (`all`: also the open ones).
pub fn achievements(s: &Services, pid: &str, all: bool, out: &mut dyn Write) -> Result<()> {
    let names = s.assets.achievement_names("en-US");
    let mut products = s.repos.achievement_products(pid)?;
    if products.is_empty() {
        products.push(("STP:PC".into(), 0));
    }
    for (product, n) in products {
        let earned = s.repos.achievements(pid, &product)?;
        let steep = product == "STP:PC" && !names.is_empty();
        let of = if steep { format!(" of {}", names.len()) } else { String::new() };
        writeln!(out, "{product}: {n} earned{of}")?;
        for &(id, ts) in &earned {
            let name = match names.iter().find(|x| x.0 == id) {
                Some((_, name, _)) if steep => format!("  {name}"),
                _ => String::new(),
            };
            let when = chrono::DateTime::from_timestamp(ts, 0).context("achievement time out of range")?;
            writeln!(out, "  {id:4}  {} UTC{name}", when.format("%Y-%m-%d %H:%M"))?;
        }
        if steep && all {
            writeln!(out, "  not yet earned:")?;
            for (id, name, text) in names.iter().filter(|x| !earned.iter().any(|e| e.0 == x.0)) {
                writeln!(out, "  {id:4}  {name} - {text}")?;
            }
        }
    }
    Ok(())
}

/// Profile entities (rider card, favorites).
pub fn entities(s: &Services, pid: &str, out: &mut dyn Write) -> Result<()> {
    for e in s.entities.for_profile(pid, None, None, 0, 1000)? {
        let keys: Vec<&str> = match e.get("obj") {
            Some(Value::Object(obj)) => obj.keys().take(6).map(String::as_str).collect(),
            _ => vec![],
        };
        writeln!(out, "{}  {:20} rev {:<4} {}  obj keys: {}", py_get(&e, "entityId"), py_get(&e, "type"),
                 py_get(&e, "revision"), py_get(&e, "lastModified"), keys.join(", "))?;
    }
    Ok(())
}

/// One's own recorded progress into the database. Ok(false): refused (the reason is printed).
pub fn import(s: &Services, recordings: &Path, replace: bool, out: &mut dyn Write) -> Result<bool> {
    let r = match import_recordings(s, recordings, replace) {
        Ok(r) => r,
        Err(e) => {
            let refused = e.downcast::<ImportError>()?;
            writeln!(out, "{refused}")?;
            return Ok(false);
        }
    };
    writeln!(out, "imported {} ({}): {} items, {} credits, {} stats, {} entities, {} achievements; now the default \
                   player", r.name, r.profile_id, r.items, r.credits, r.stats, r.entities, r.achievements)?;
    Ok(true)
}

pub fn reset(s: &Services, pid: &str, out: &mut dyn Write) -> Result<()> {
    s.reset(pid)?;
    writeln!(out, "reset {pid}: starts as a new player on the next login")?;
    Ok(())
}

/// Search the store catalogue by name (nameId, case-insensitive).
pub fn offers(s: &Services, text: &str, out: &mut dyn Write) -> Result<()> {
    let text = text.to_lowercase();
    for o in s.assets.catalog.all() {
        let name = o.get("nameId").and_then(Value::as_str).unwrap_or("");
        if !name.to_lowercase().contains(&text) {
            continue;
        }
        // {itemId[:8]: quantity}, as a Python dict prints
        let mut cost = Map::new();
        for i in o.get("items").and_then(Value::as_array).into_iter().flatten() {
            let id = i.get("itemId").and_then(Value::as_str).unwrap_or("");
            cost.insert(id.chars().take(8).collect(), i.get("quantity").cloned().unwrap_or(Value::Null));
        }
        let o = o.as_object().cloned().unwrap_or_default();
        writeln!(out, "{}  {:11} {}  {}", py_get(&o, "offerId"), py_get(&o, "type"), py_get(&o, "nameId"),
                 py_repr(&Value::Object(cost)))?;
    }
    Ok(())
}

// ---- Python's str() and format() of values ------------------------------------------------------------------------
/// str(d.get(key)).
fn py_get(m: &Map<String, Value>, key: &str) -> String {
    py_str(m.get(key).unwrap_or(&Value::Null))
}

/// str(value) of a JSON value as Python holds it.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => py_repr(v),
    }
}

/// repr(value) of a JSON value as Python holds it (None, True, 'text', [..], {..}).
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) if n.is_f64() => crate::util::py_float(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
            let mut out = String::from(quote);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == quote => out.extend(['\\', c]),
                    c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", py_repr(&Value::String(k.clone())),
                                                                            py_repr(v))).collect::<Vec<_>>().join(", ")),
    }
}

/// format(f, "g"): 6 significant digits, trailing zeros dropped, scientific below 1e-4 and from 1e6.
pub fn format_g(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf" } else { "-inf" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = format!("{f:.5e}"); // rounded to 6 significant digits: "1.23457e6"
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() };
    if (-4..6).contains(&exp) {
        strip(&format!("{f:.*}", (5 - exp) as usize))
    } else {
        format!("{}e{}{:02}", strip(mantissa), if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

// ---- access: bans, the whitelist, kicks (services/access.rs) ------------------------------------------------------
/// A player as an admin types them: profile id, account email or player name.
fn find(s: &Services, player: &str) -> Result<String> {
    s.accounts.access.find_player(player)?.with_context(|| format!("no player {player:?} (a profile id, an account's \
                                                                     email or a player's name)"))
}

fn duration(text: Option<&str>) -> Result<Option<i64>> {
    crate::services::access::parse_duration(text.unwrap_or(""))
        .with_context(|| format!("not a duration: {:?} (e.g. 30m, 12h, 7d, 2w)", text.unwrap_or("")))
}

fn until(expires: Option<i64>) -> String {
    expires.map(|t| format!("until {}", crate::services::access::format_time(t))).unwrap_or_else(|| "permanent".into())
}

const RUNNING: &str = "(a running server applies it within a few seconds)";

pub fn bans(s: &Services, out: &mut dyn Write) -> Result<()> {
    use crate::services::access::{ALLOW_PLAYER, BAN_IP, BAN_PLAYER};
    let access = &s.accounts.access;
    writeln!(out, "whitelist: {}", if access.whitelist_enabled()? { "on" } else { "off" })?;
    let rules = access.list()?;
    let name = |pid: &str| -> Result<String> { Ok(s.repos.player(pid)?.map(|p| p.name).unwrap_or_default()) };
    for r in &rules {
        let reason = if r.reason.is_empty() { String::new() } else { format!("  {:?}", r.reason) };
        match r.kind.as_str() {
            BAN_PLAYER => writeln!(out, "banned   {}  {:16} {}  by {}{reason}", r.value, name(&r.value)?, until(r.expires),
                                   r.by)?,
            BAN_IP => writeln!(out, "banned   address {:24} {}  by {}{reason}", r.value, until(r.expires), r.by)?,
            ALLOW_PLAYER => writeln!(out, "allowed  {}  {:16} by {}", r.value, name(&r.value)?, r.by)?,
            _ => {}
        }
    }
    if rules.is_empty() {
        writeln!(out, "no bans, nobody on the whitelist")?;
    }
    Ok(())
}

pub fn ban(s: &Services, player: &str, length: Option<&str>, reason: &str, out: &mut dyn Write) -> Result<bool> {
    let (pid, seconds) = (find(s, player)?, duration(length)?);
    match s.accounts.access.ban_player(&pid, reason, seconds, "cli")? {
        Ok(()) => {
            let expires = seconds.map(|t| crate::util::unix_now() as i64 + t);
            writeln!(out, "banned {pid}, {} {RUNNING}", until(expires))?;
            Ok(true)
        }
        Err(why) => {
            writeln!(out, "{why}")?;
            Ok(false)
        }
    }
}

pub fn unban(s: &Services, player: &str, out: &mut dyn Write) -> Result<bool> {
    let pid = find(s, player)?;
    let had = s.accounts.access.unban_player(&pid, "cli")?;
    writeln!(out, "{}", if had { format!("lifted the ban of {pid}") } else { format!("{pid} is not banned") })?;
    Ok(had)
}

pub fn ban_ip(s: &Services, address: &str, length: Option<&str>, reason: &str, out: &mut dyn Write) -> Result<bool> {
    let seconds = duration(length)?;
    match s.accounts.access.ban_address(address, reason, seconds, "cli")? {
        Ok(range) => {
            let expires = seconds.map(|t| crate::util::unix_now() as i64 + t);
            writeln!(out, "banned the address {range}, {} {RUNNING}", until(expires))?;
            Ok(true)
        }
        Err(why) => {
            writeln!(out, "{why}")?;
            Ok(false)
        }
    }
}

pub fn unban_ip(s: &Services, address: &str, out: &mut dyn Write) -> Result<bool> {
    match s.accounts.access.unban_address(address, "cli")? {
        Ok(true) => writeln!(out, "lifted the ban of {address}")?,
        Ok(false) => writeln!(out, "{address} is not banned (`admin bans` lists the bans)")?,
        Err(why) => writeln!(out, "{why}")?,
    }
    Ok(true)
}

pub fn kick(s: &Services, player: &str, reason: &str, out: &mut dyn Write) -> Result<bool> {
    let pid = find(s, player)?;
    s.accounts.access.request_kick(&pid, reason)?;
    writeln!(out, "kick of {pid} requested {RUNNING}")?;
    Ok(true)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Whitelist {
    On,
    Off,
    Add,
    Remove,
    List,
}

pub fn whitelist(s: &Services, action: Whitelist, player: Option<&str>, out: &mut dyn Write) -> Result<bool> {
    let access = &s.accounts.access;
    let player = |verb: &str| -> Result<String> {
        find(s, player.with_context(|| format!("whitelist {verb} needs a player"))?)
    };
    match action {
        Whitelist::On => {
            access.set_whitelist(true, "cli")?;
            let n = access.list()?.iter().filter(|r| r.kind == crate::services::access::ALLOW_PLAYER).count();
            writeln!(out, "whitelist on: {n} player(s) and the admins get in {RUNNING}")?;
        }
        Whitelist::Off => {
            access.set_whitelist(false, "cli")?;
            writeln!(out, "whitelist off: every player gets in (bans still apply)")?;
        }
        Whitelist::Add => {
            let pid = player("add")?;
            access.allow(&pid, "cli")?;
            writeln!(out, "{pid} is on the whitelist")?;
        }
        Whitelist::Remove => {
            let pid = player("remove")?;
            let had = access.disallow(&pid)?;
            writeln!(out, "{}", if had { format!("{pid} is off the whitelist") } else { format!("{pid} was not on it") })?;
        }
        Whitelist::List => return bans(s, out).map(|_| true),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{HttpFixture, HttpFixtures, OfferCatalog};
    use crate::services::importer::synthetic as syn;
    use serde_json::json;

    fn services(assets: Assets) -> Services {
        let db = Arc::new(Db::memory().unwrap());
        db.migrate().unwrap();
        Services::new(Arc::new(Config::default()), Arc::new(assets), Arc::new(Repos::new(db)), None)
    }

    /// Services with the made-up recorded player imported.
    fn imported(assets: Assets) -> (tempfile::TempDir, Services) {
        let dir = tempfile::tempdir().unwrap();
        syn::build(dir.path());
        let s = services(assets);
        import_recordings(&s, dir.path(), false).unwrap();
        (dir, s)
    }

    fn text(f: impl FnOnce(&mut dyn Write) -> Result<()>) -> String {
        let mut out = vec![];
        f(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn grant_arguments() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(grant_args(&v(&["credits", "-500"])).unwrap(), (None, "credits".into(), -500));
        assert_eq!(grant_args(&v(&["p1", "credits", "5"])).unwrap(), (Some("p1".into()), "credits".into(), 5));
        assert_eq!(grant_args(&v(&["credits", "5", "x"])).unwrap(), (None, "credits".into(), 5));
        assert!(grant_args(&v(&["credits", "five"])).is_err() && grant_args(&v(&["credits"])).is_err());
    }

    #[test]
    fn import_then_inspect() {
        let dir = tempfile::tempdir().unwrap();
        syn::build(dir.path());
        let s = services(Assets::default());
        let mut out = vec![];
        assert!(import(&s, dir.path(), false, &mut out).unwrap());
        assert_eq!(String::from_utf8(out).unwrap(),
                   format!("imported {} ({}): 3 items, 50600 credits, 2 stats, 1 entities, 3 achievements; now the \
                            default player\n", syn::PLAYER_NAME, syn::PLAYER));
        let mut out = vec![];
        assert!(!import(&s, dir.path(), false, &mut out).unwrap());
        assert_eq!(String::from_utf8(out).unwrap(),
                   format!("player {} ({}) exists; use --replace to overwrite\n", syn::PLAYER_NAME, syn::PLAYER));

        let pid = player_id(&s, None).unwrap();
        assert_eq!(pid, syn::PLAYER);
        assert_eq!(player_id(&s, Some("ABC")).unwrap(), "abc");
        let line = text(|o| players(&s, o));
        assert!(line.starts_with(&format!("{}  {:24} level 41  credits 50600  since ", syn::PLAYER, syn::PLAYER_NAME)),
                "{line}");
        assert_eq!(text(|o| inventory(&s, &pid, None, o)), "3 items, 50600 credits\n");
        assert_eq!(text(|o| inventory(&s, &pid, Some("0000000A-0000-4000-8000-00000000C"), o)),
                   format!("{}  quantity 1  (modified 2026-01-01T01:00:00.000Z)\n", syn::BADGE_ITEM));
        assert_eq!(text(|o| inventory(&s, &pid, Some("ffff"), o)), "not owned\n");
        assert_eq!(text(|o| stats(&s, &pid, Some("level"), None, o)), format!("{:48} 41\n", "level_stat:6:infinite"));
        assert_eq!(text(|o| stats(&s, &pid, Some("xp_ratio"), Some("xp_ratio_stat:0:infinite=0.25"), o)),
                   format!("{:48} 0.25\n", "xp_ratio_stat:0:infinite"));
        assert_eq!(text(|o| stats(&s, &pid, Some("level"), Some("level_stat:6:infinite=42"), o)),
                   format!("{:48} 42\n", "level_stat:6:infinite"));
        assert!(stats(&s, &pid, None, Some("level_stat:6:infinite=x"), &mut vec![]).is_err());
        assert_eq!(text(|o| entities(&s, &pid, o)),
                   format!("{}  {:20} rev 3    2026-01-01T00:00:00.000Z  obj keys: RIDER_CARD, GLOBAL_VERSION\n",
                           syn::CARD, "PROFILE"));
        let ledger_line = text(|o| ledger(&s, &pid, 20, o));
        assert!(ledger_line.ends_with(&format!("  import     {CREDITS_ITEM}  +50600 -> 50600\n")), "{ledger_line}");
    }

    #[test]
    fn grant_and_its_ledger() {
        let (_dir, s) = imported(Assets::default());
        assert_eq!(text(|o| grant(&s, syn::PLAYER, "CREDITS", -600, o).map(|_| ())),
                   format!("{CREDITS_ITEM}: 50600 -> 50000\n"));
        let mut out = vec![];
        assert!(!grant(&s, syn::PLAYER, "credits", -60_000, &mut out).unwrap());
        assert!(String::from_utf8(out).unwrap().starts_with("would go negative"));
        // a player who does not exist yet is created first
        let other = "00000000-0000-4000-8000-0000000000aa";
        assert_eq!(text(|o| grant(&s, other, "0000000A-0000-4000-8000-00000000A001", 1, o).map(|_| ())),
                   "0000000a-0000-4000-8000-00000000a001: 0 -> 1\n");
        let line = text(|o| ledger(&s, syn::PLAYER, 1, o));
        assert!(line.ends_with(&format!("  admin      {CREDITS_ITEM}  -600 -> 50000\n")), "{line}");
        s.repos.add_ledger(syn::PLAYER, "x", 1, 1, "purchase", Some("offer-1")).unwrap();
        assert!(text(|o| ledger(&s, syn::PLAYER, 1, o)).ends_with("  purchase   x  +1 -> 1  offer offer-1\n"));
    }

    #[test]
    fn events_and_scores() {
        let (_dir, s) = imported(Assets::default());
        s.repos.add_stat_event(syn::PLAYER, "update_xp", &json!({"xp": 5})).unwrap();
        assert!(text(|o| events(&s, syn::PLAYER, 20, o)).ends_with(&format!("  {:32} {{\"xp\": 5}}\n", "update_xp")));
        s.repos.set_score(syn::PLAYER, "race_1", "max", 1234.5, None).unwrap();
        s.repos.set_score(syn::PLAYER, "race_2", "min", 12345678.0, None).unwrap();
        let lines = text(|o| scores(&s, syn::PLAYER, o));
        let lines: Vec<&str> = lines.lines().collect();
        assert!(lines[0].starts_with(&format!("{:48} {:>12}  (max, ", "race_1", "1234.5")), "{}", lines[0]);
        assert!(lines[1].starts_with(&format!("{:48} {:>12}  (min, ", "race_2", "1.23457e+07")), "{}", lines[1]);
    }

    fn achievement_pack() -> Assets {
        let mut buf = std::io::Cursor::new(vec![]);
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("en-US_loc.txt", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all("\u{feff}1\tFirst steps\tDo the first thing\n4\tFour\tDo a fourth thing\n15\tFifteen\tDo it \
                         fifteen times\n30\tThirty\tNot earned yet\n".as_bytes()).unwrap();
            z.finish().unwrap();
        }
        let entry = HttpFixture { method: "GET".into(), host: "static3.cdn.ubi.com".into(),
                                  path: "/orbit/uplay_launcher_3_0/assets/0123abcd.zip".into(), status: 200,
                                  content_type: "application/zip".into(), body: buf.into_inner() };
        Assets { connect_http: HttpFixtures::new(vec![entry]), ..Assets::default() }
    }

    #[test]
    fn achievements_with_names() {
        let (_dir, s) = imported(achievement_pack());
        assert_eq!(text(|o| achievements(&s, syn::PLAYER, true, o)),
                   "STP:PC: 3 earned of 4\n\
                    \x20    1  2017-07-14 02:40 UTC  First steps\n\
                    \x20    4  2017-07-14 02:41 UTC  Four\n\
                    \x20   15  2017-07-14 02:43 UTC  Fifteen\n\
                    \x20 not yet earned:\n\
                    \x20   30  Thirty - Not earned yet\n");
        let fresh = services(Assets::default());
        assert_eq!(text(|o| achievements(&fresh, syn::PLAYER, true, o)), "STP:PC: 0 earned\n");
    }

    #[test]
    fn reset_starts_over_as_a_new_player_and_forgets_achievements() {
        let (_dir, s) = imported(Assets::default());
        assert_eq!(text(|o| reset(&s, syn::PLAYER, o)),
                   format!("reset {}: starts as a new player on the next login\n", syn::PLAYER));
        assert!(s.accounts.player(syn::PLAYER).unwrap().name.starts_with("Player-"));
        assert_eq!(s.economy.credits(syn::PLAYER).unwrap(), 0);
        assert!(s.repos.achievement_products(syn::PLAYER).unwrap().is_empty());
        assert!(s.repos.entities_of(syn::PLAYER, None).unwrap().is_empty());
    }

    #[test]
    fn resetting_a_player_keeps_its_account() {
        let (_dir, s) = imported(Assets::default());
        let acc = s.accounts.register("host@example.com", "a long enough secret", "").unwrap();
        assert_eq!(acc.profile_id, syn::PLAYER); // took over the imported player
        s.reset(&acc.profile_id).unwrap();
        assert_eq!(s.accounts.authenticate("host@example.com", "a long enough secret").unwrap().profile_id,
                   acc.profile_id);
        assert_eq!(s.repos.player(&acc.profile_id).unwrap().unwrap().name, syn::PLAYER_NAME);
        assert!(s.accounts.required().unwrap());
        assert_eq!(s.repos.quantity(&acc.profile_id, CREDITS_ITEM).unwrap(), 0);
    }

    #[test]
    fn cloud_saves_listing() {
        let (_dir, s) = imported(Assets::default());
        assert_eq!(text(|o| cloud_saves(&s, syn::PLAYER, None, false, o)), "no cloud saves\n");
        let mut e = Map::new();
        for (k, v) in [("entityId", json!("0000000d-0000-4000-8000-000000000001")), ("profileId", json!(syn::PLAYER)),
                       ("type", json!("cloudSave")), ("name", json!(crate::services::cloudsaves::entity_name(3280))),
                       ("obj", json!({"slots": [{"filename": "1.save", "size": 12}, "junk"]})),
                       ("lastModified", json!("2026-01-01T00:00:00.000Z")), ("revision", json!(2))] {
            e.insert(k.into(), v);
        }
        s.repos.entity_put(&e).unwrap();
        let line = text(|o| cloud_saves(&s, syn::PLAYER, Some(syn::PLAYER), false, o));
        assert!(line.starts_with(&format!("{}  product 3280  revision 2  2026-01-01T00:00:00.000Z  ", syn::PLAYER)),
                "{line}");
        assert!(line.ends_with("  1.save (12 B)\n"), "{line}");
        assert_eq!(text(|o| cloud_saves(&s, syn::PLAYER, None, true, o)),
                   format!("removed 1 cloud save(s) of {}\n", syn::PLAYER));
        assert_eq!(text(|o| cloud_saves(&s, syn::PLAYER, None, false, o)), "no cloud saves\n");
    }

    #[test]
    fn offers_by_name() {
        let offer = json!({"offerId": "0000000b-0000-4000-8000-0000000000a1", "type": "shop", "nameId": "TEST_A1",
                           "items": [{"itemId": "0000000a-0000-4000-8000-00000000a001", "quantity": 1},
                                     {"itemId": CREDITS_ITEM, "quantity": -12000}]});
        let s = services(Assets { catalog: OfferCatalog::new(vec![offer]), ..Assets::default() });
        assert_eq!(text(|o| offers(&s, "test_a", o)),
                   "0000000b-0000-4000-8000-0000000000a1  shop        TEST_A1  {'0000000a': 1, 'e3570d44': -12000}\n");
        assert_eq!(text(|o| offers(&s, "nothing", o)), "");
    }

    #[test]
    fn python_formatting() {
        for (f, want) in [(1234.5, "1234.5"), (12.0, "12"), (123456.0, "123456"), (1234567.0, "1.23457e+06"),
                          (0.0001, "0.0001"), (0.00001, "1e-05"), (-2.5, "-2.5"), (0.0, "0"), (1e100, "1e+100"),
                          (999999.5, "1e+06"), (f64::INFINITY, "inf")] {
            assert_eq!(format_g(f), want, "{f}");
        }
        assert_eq!(py_repr(&json!({"a": [1, 2.0, null, true], "it's": "x"})), "{'a': [1, 2.0, None, True], \"it's\": 'x'}");
        assert_eq!(py_str(&json!("text")), "text");
    }

    #[test]
    fn open_moves_a_database_from_before_the_rename() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.server.data_dir = dir.path().to_path_buf();
        config.server.use_gamedata = false;
        {
            let s = open(config.clone()).unwrap();
            s.repos.create_player(syn::PLAYER, syn::PLAYER, syn::PLAYER_NAME).unwrap();
        }
        for suffix in ["", "-wal", "-shm"] {
            let src = dir.path().join(format!("powder.db{suffix}"));
            if src.exists() {
                std::fs::rename(&src, dir.path().join(format!("steep.db{suffix}"))).unwrap();
            }
        }
        let s = open(config).unwrap();
        assert!(dir.path().join("powder.db").exists() && !dir.path().join("steep.db").exists());
        assert!(s.repos.player(syn::PLAYER).unwrap().is_some());
    }
}
