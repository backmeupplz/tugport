//! The panel API's request log: every `/api` request with its route, status, time and caller,
//! kept for a week in its own SQLite file, so it never slows Tugport's state or its backups.

use crate::app::AppState;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

const KEEP_DAYS: i64 = 7;

/// Requests on their way to the writer; unset until it runs (on other servers, never).
static TX: OnceLock<mpsc::Sender<Entry>> = OnceLock::new();

pub struct Entry {
    pub at: i64,
    pub method: String,
    pub route: String,
    pub path: String,
    pub query: Option<String>,
    pub status: u16,
    pub us: u64,
    pub bytes: Option<u64>,
    pub who: Option<Caller>,
    pub ip: Option<String>,
    pub agent: Option<String>,
}

/// A caller's name and the id of the API key it used.
pub type Caller = (String, Option<String>);

/// Who made a request: the auth extractor fills it in.
#[derive(Clone, Default)]
pub struct Who(pub Arc<Mutex<Option<Caller>>>);

pub fn enabled() -> bool {
    TX.get().is_some()
}

/// Queue a request for the log; dropped when the writer is far behind.
pub fn record(e: Entry) {
    if let Some(tx) = TX.get() {
        let _ = tx.try_send(e);
    }
}

fn path(app: &AppState) -> PathBuf {
    app.paths.data.join("requests.db")
}

/// Write queued requests about once a second and drop the ones older than a week.
pub async fn run(app: AppState) {
    let conn = match open(&path(&app)) {
        Ok(c) => c,
        Err(e) => return tracing::warn!("request log disabled: {e:#}"),
    };
    let (tx, mut rx) = mpsc::channel(10_000);
    if TX.set(tx).is_err() {
        return;
    }
    let mut pruned = 0;
    let mut batch = Vec::with_capacity(1000);
    while rx.recv_many(&mut batch, 1000).await > 0 {
        if let Err(e) = insert(&conn, &batch) {
            tracing::warn!("writing the request log: {e:#}");
        }
        batch.clear();
        let now = crate::util::now();
        if now - pruned > 3600 {
            pruned = now;
            let _ = conn.execute("DELETE FROM requests WHERE at < ?1", [(now - KEEP_DAYS * 86400) * 1000]);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS requests (
             id INTEGER PRIMARY KEY, at INTEGER NOT NULL, method TEXT NOT NULL, route TEXT NOT NULL, path TEXT NOT NULL,
             query TEXT, status INTEGER NOT NULL, us INTEGER NOT NULL, bytes INTEGER, who TEXT, key_id TEXT, ip TEXT, agent TEXT);
         CREATE INDEX IF NOT EXISTS requests_at ON requests (at);",
    )?;
    Ok(conn)
}

fn insert(conn: &Connection, batch: &[Entry]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO requests (at, method, route, path, query, status, us, bytes, who, key_id, ip, agent) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        for e in batch {
            let (who, key) = e.who.clone().unzip();
            stmt.execute(params![e.at, e.method, e.route, e.path, e.query, e.status, e.us, e.bytes, who, key.flatten(), e.ip, e.agent])?;
        }
    }
    Ok(tx.commit()?)
}

/// Query values whose names look secret are left out (`?token=…`).
pub fn redact_query(q: &str) -> String {
    const SECRET: &[&str] = &["token", "key", "secret", "password", "code", "state", "sig"];
    q.split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if SECRET.iter().any(|s| k.to_ascii_lowercase().contains(s)) => format!("{k}=…"),
            _ => kv.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Run a read of the log off the async threads, on its own read-only connection.
pub async fn read<T: Send + 'static>(app: &AppState, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
    let path = path(app);
    tokio::task::spawn_blocking(move || {
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        f(&conn)
    })
    .await?
}

#[derive(serde::Serialize, Debug, PartialEq)]
pub struct RouteStats {
    pub method: String,
    pub route: String,
    pub count: usize,
    /// Answers with a 5xx status.
    pub errors: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub total_ms: f64,
}

/// Per route: how often it was called since `since_ms` and how long it took, slowest p95 first.
pub fn summary(conn: &Connection, since_ms: i64) -> Result<Vec<RouteStats>> {
    let mut stmt = conn.prepare("SELECT method, route, status, us FROM requests WHERE at >= ?1")?;
    let mut groups: HashMap<(String, String), (Vec<u64>, usize)> = HashMap::new();
    let rows = stmt.query_map([since_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, u16>(2)?, r.get::<_, u64>(3)?)))?;
    for row in rows {
        let (method, route, status, us) = row?;
        let g = groups.entry((method, route)).or_default();
        g.0.push(us);
        g.1 += usize::from(status >= 500);
    }
    Ok(stats(groups))
}

fn stats(groups: HashMap<(String, String), (Vec<u64>, usize)>) -> Vec<RouteStats> {
    let ms = |us: u64| (us as f64 / 100.0).round() / 10.0;
    let mut out: Vec<RouteStats> = groups
        .into_iter()
        .map(|((method, route), (mut us, errors))| {
            us.sort_unstable();
            // Nearest rank: the smallest time at least p% of the requests took at most.
            let at = |p: usize| ms(us[(us.len() * p).div_ceil(100).saturating_sub(1)]);
            RouteStats {
                count: us.len(),
                errors,
                p50_ms: at(50),
                p95_ms: at(95),
                p99_ms: at(99),
                max_ms: ms(us[us.len() - 1]),
                total_ms: ms(us.iter().sum()),
                method,
                route,
            }
        })
        .collect();
    out.sort_by(|a, b| b.p95_ms.total_cmp(&a.p95_ms));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_per_route() {
        let g = HashMap::from([
            (("GET".to_string(), "/api/a".to_string()), ((1..=100).map(|ms| ms * 1000).collect(), 2)),
            (("GET".to_string(), "/api/b".to_string()), (vec![500], 0)),
        ]);
        let s = stats(g);
        assert_eq!(s[0].route, "/api/a");
        assert_eq!((s[0].count, s[0].errors, s[0].p50_ms, s[0].p95_ms, s[0].p99_ms, s[0].max_ms), (100, 2, 50.0, 95.0, 99.0, 100.0));
        assert_eq!((s[1].p50_ms, s[1].max_ms), (0.5, 0.5));
    }

    #[test]
    fn secret_query_values_are_left_out() {
        assert_eq!(redact_query("since=5&token=abc&api_key=x&level=info"), "since=5&token=…&api_key=…&level=info");
    }

    #[test]
    fn writes_and_reads_back() {
        let conn = open(Path::new(":memory:")).unwrap();
        let e = |route: &str, us| Entry {
            at: 1000,
            method: "GET".into(),
            route: route.into(),
            path: route.into(),
            query: None,
            status: 200,
            us,
            bytes: Some(10),
            who: Some(("nikita".into(), None)),
            ip: None,
            agent: None,
        };
        insert(&conn, &[e("/api/a", 2000), e("/api/a", 4000)]).unwrap();
        let s = summary(&conn, 0).unwrap();
        assert_eq!((s.len(), s[0].count, s[0].max_ms), (1, 2, 4.0));
        assert!(summary(&conn, 2000).unwrap().is_empty());
    }
}
