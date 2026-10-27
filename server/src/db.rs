//! SQLite access: one connection, shared by every task (calls are sub-millisecond at this size).
//!
//! Migrations are numbered SQL files in server/migrations/ (0001_init.sql, ...), compiled in. A database's
//! `PRAGMA user_version` records the last applied number; newer ones are applied in order, each in its own
//! transaction. WAL mode lets the admin commands read and write while the server is running.
//!
//! Transactions nest: an inner `tx` joins the outer one (registering an account creates its player inside
//! the same transaction). The lock is reentrant for that reason; nothing awaits while holding it.

use anyhow::{Context, Result};
use parking_lot::ReentrantMutex;
use rusqlite::{Connection, Params, Row};
use std::cell::{Cell, RefCell};
use std::path::Path;

pub const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_progression.sql")),
    (3, include_str!("../migrations/0003_achievements.sql")),
    (4, include_str!("../migrations/0004_accounts.sql")),
    (5, include_str!("../migrations/0005_ugc.sql")),
    (6, include_str!("../migrations/0006_access.sql")),
];

struct Inner {
    conn: RefCell<Connection>,
    depth: Cell<u32>,
}

pub struct Db {
    inner: ReentrantMutex<Inner>,
}

// The connection is only ever used while holding the (reentrant, per-thread) lock.
unsafe impl Sync for Db {}
unsafe impl Send for Db {}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::with_connection(conn)
    }

    pub fn memory() -> Result<Db> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(Db { inner: ReentrantMutex::new(Inner { conn: RefCell::new(conn), depth: Cell::new(0) }) })
    }

    /// Run `f` with the connection (one statement or a few reads; use `tx` for atomic changes).
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let guard = self.inner.lock();
        let conn = guard.conn.borrow();
        Ok(f(&conn)?)
    }

    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        self.with(|c| c.execute(sql, params))
    }

    pub fn query<T>(&self, sql: &str, params: impl Params, map: impl FnMut(&Row) -> rusqlite::Result<T>)
                    -> Result<Vec<T>> {
        self.with(|c| {
            let mut stmt = c.prepare_cached(sql)?;
            let rows = stmt.query_map(params, map)?;
            rows.collect()
        })
    }

    pub fn query_one<T>(&self, sql: &str, params: impl Params, map: impl FnOnce(&Row) -> rusqlite::Result<T>)
                        -> Result<Option<T>> {
        self.with(|c| {
            let mut stmt = c.prepare_cached(sql)?;
            let mut rows = stmt.query(params)?;
            match rows.next()? {
                Some(row) => Ok(Some(map(row)?)),
                None => Ok(None),
            }
        })
    }

    /// Atomic block; nests (inner blocks join the outer transaction). An error rolls the whole outer
    /// transaction back.
    pub fn tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let guard = self.inner.lock();
        let depth = guard.depth.get();
        if depth > 0 {
            guard.depth.set(depth + 1);
            let out = f();
            guard.depth.set(depth);
            return out;
        }
        guard.conn.borrow().execute_batch("BEGIN IMMEDIATE")?;
        guard.depth.set(1);
        let out = f();
        guard.depth.set(0);
        let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
        guard.conn.borrow().execute_batch(end)?;
        out
    }

    pub fn version(&self) -> Result<u32> {
        self.with(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
    }

    /// Apply pending migrations; returns the numbers applied.
    pub fn migrate(&self) -> Result<Vec<u32>> {
        let current = self.version()?;
        let mut applied = vec![];
        for (num, sql) in MIGRATIONS {
            if *num <= current {
                continue;
            }
            self.tx(|| {
                self.with(|c| c.execute_batch(sql))?;
                self.with(|c| c.execute_batch(&format!("PRAGMA user_version = {num}")))?;
                Ok(())
            }).with_context(|| format!("migration {num}"))?;
            applied.push(*num);
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_and_nested_transactions() {
        let db = Db::memory().unwrap();
        assert_eq!(db.migrate().unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert!(db.migrate().unwrap().is_empty());
        let r: Result<()> = db.tx(|| {
            db.execute("INSERT INTO kv(scope, key, value_json) VALUES ('s', 'a', '1')", [])?;
            db.tx(|| {
                db.execute("INSERT INTO kv(scope, key, value_json) VALUES ('s', 'b', '2')", [])?;
                Ok(())
            })?;
            anyhow::bail!("boom")
        });
        assert!(r.is_err());
        let n: i64 = db.with(|c| c.query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))).unwrap();
        assert_eq!(n, 0, "the inner insert is rolled back with the outer transaction");
    }
}
