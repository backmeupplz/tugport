use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_init.sql"),
    include_str!("migrations/002_cloud.sql"),
    include_str!("migrations/003_ingress.sql"),
];

/// SQLite in WAL mode behind a mutex. Calls are short; never hold the guard across `.await`.
pub struct Db(Mutex<Connection>);

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL;",
        )?;
        migrate(&mut conn)?;
        Ok(Db(Mutex::new(conn)))
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        crate::util::lock(&self.0)
    }

    /// Every row of a query, mapped by `f`.
    pub fn rows<T>(&self, sql: &str, params: impl Params, f: impl FnMut(&Row) -> rusqlite::Result<T>) -> rusqlite::Result<Vec<T>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(sql)?;
        stmt.query_map(params, f)?.collect()
    }

    /// The first row of a query, if there is one.
    pub fn row<T>(&self, sql: &str, params: impl Params, f: impl FnOnce(&Row) -> rusqlite::Result<T>) -> rusqlite::Result<Option<T>> {
        self.conn().query_row(sql, params, f).optional()
    }

    pub fn exec(&self, sql: &str, params: impl Params) -> rusqlite::Result<usize> {
        self.conn().execute(sql, params)
    }

    pub fn get_setting<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let raw: Option<String> = self.row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))?;
        Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    pub fn set_setting<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.exec(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.row("SELECT value FROM state WHERE key = ?1", [key], |r| r.get(0))?)
    }

    /// A state value parsed as `T` (`String` for the text); None when it is missing, does not
    /// parse or cannot be read.
    pub fn state<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.get_state(key).ok().flatten()?.parse().ok()
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.exec(
            "INSERT INTO state (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn del_state(&self, key: &str) -> rusqlite::Result<()> {
        self.exec("DELETE FROM state WHERE key = ?1", [key]).map(drop)
    }

    /// Consistent copy of the database for backups.
    pub fn snapshot(&self, dest: &Path) -> Result<()> {
        let _ = std::fs::remove_file(dest);
        self.conn().execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current = usize::try_from(conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?).context("the database has a negative schema version")?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql).with_context(|| format!("migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_and_stores_settings() {
        let db = Db::memory().unwrap();
        db.set_setting("deploy.policy", &"cancel").unwrap();
        assert_eq!(db.get_setting::<String>("deploy.policy").unwrap().as_deref(), Some("cancel"));
        assert_eq!(db.get_setting::<String>("missing").unwrap(), None);
        let v: i64 = db.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        db.set_state("n", "7").unwrap();
        assert_eq!(db.state::<i64>("n"), Some(7));
        assert_eq!(db.state::<String>("n").as_deref(), Some("7"));
        assert_eq!(db.state::<i64>("missing"), None);
    }
}
