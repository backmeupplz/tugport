//! Service logs: every container's output is followed, masked, and appended to
//! per-service daily files that survive restarts and redeploys. Plus Tugport's own log.

use crate::app::{App, AppState};
use crate::runtime;
use crate::secrets::Masker;
use crate::util;
use anyhow::Result;
use bollard::query_parameters::{ListContainersOptions, LogsOptions};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    #[serde(skip)]
    pub service_id: String,
    pub ts: i64,
    pub replica: String,
    pub stream: &'static str,
    pub line: String,
}

pub fn service_dir(app: &App, service_id: &str) -> PathBuf {
    app.paths.logs().join(service_id)
}

fn day_file(dir: &Path, ts_ms: i64) -> PathBuf {
    let day = chrono::DateTime::from_timestamp_millis(ts_ms).unwrap_or_default().format("%Y-%m-%d");
    dir.join(format!("{day}.log"))
}

/// Follow every managed container on every server (other servers' output comes over the
/// mesh, so logs are stored and searched in one place); new containers are picked up within seconds.
pub async fn collect(app: AppState) {
    let following: Arc<Mutex<HashSet<String>>> = Default::default();
    crate::nodes::for_each_node(app, move |app, node| collect_on(app, node.docker.clone(), following.clone())).await;
}

async fn collect_on(app: AppState, docker: bollard::Docker, following: Arc<Mutex<HashSet<String>>>) {
    loop {
        let filters = HashMap::from([("label".to_string(), vec![format!("{}=true", runtime::L_MANAGED)])]);
        let Ok(list) = docker.list_containers(Some(ListContainersOptions { filters: Some(filters), ..Default::default() })).await else { return };
        for c in list {
            let (Some(id), Some(labels)) = (c.id, c.labels) else { continue };
            let Some(service) = labels.get(runtime::L_SERVICE).cloned() else { continue };
            if crate::util::lock(&following).contains(&id) {
                continue;
            }
            // Labels on other servers come from their Docker: the id names a folder here, so
            // follow only services that exist.
            if !crate::store::service(&app, &service).is_ok_and(|s| s.is_some_and(|s| s.id == service)) {
                continue;
            }
            if !crate::util::lock(&following).insert(id.clone()) {
                continue;
            }
            let name = c.names.and_then(|n| n.first().cloned()).unwrap_or_default().trim_start_matches('/').to_string();
            let (app, docker, following) = (app.clone(), docker.clone(), following.clone());
            tokio::spawn(async move {
                follow(&app, &docker, &id, &name, &service).await;
                crate::util::lock(&following).remove(&id);
            });
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn follow(app: &AppState, docker: &bollard::Docker, container: &str, name: &str, service_id: &str) {
    let pos_key = format!("logpos:{container}");
    let last_ms: i64 = app.db.get_state(&pos_key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
    let since = if last_ms > 0 { (last_ms / 1000) as i32 } else { (util::now() - 5) as i32 };
    let mut stream = docker.logs(
        container,
        Some(LogsOptions { follow: true, stdout: true, stderr: true, timestamps: true, since, ..Default::default() }),
    );
    let dir = service_dir(app, service_id);
    let _ = std::fs::create_dir_all(&dir);
    let mut masker = masker_fn(app, service_id);
    let mut masker_at = std::time::Instant::now();
    let mut newest = last_ms;
    let mut saved_at = std::time::Instant::now();
    let mut file: Option<(PathBuf, std::fs::File)> = None;
    while let Some(Ok(chunk)) = stream.next().await {
        let (stream_name, bytes) = match chunk {
            bollard::container::LogOutput::StdErr { message } => ("stderr", message),
            bollard::container::LogOutput::StdOut { message } | bollard::container::LogOutput::Console { message } => ("stdout", message),
            _ => continue,
        };
        if masker_at.elapsed() > Duration::from_secs(60) {
            masker = masker_fn(app, service_id);
            masker_at = std::time::Instant::now();
        }
        for raw in String::from_utf8_lossy(&bytes).lines() {
            let (ts, text) = split_timestamp(raw);
            if ts <= last_ms {
                continue;
            }
            let text = masker.mask(text).into_owned();
            newest = newest.max(ts);
            let path = day_file(&dir, ts);
            if file.as_ref().is_none_or(|(p, _)| *p != path) {
                file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok().map(|f| (path, f));
            }
            if let Some((_, f)) = file.as_mut() {
                let _ = writeln!(f, "{ts}\t{name}\t{stream_name}\t{}", text.replace('\t', "    "));
            }
            let _ = app.logs.send(LogLine { service_id: service_id.to_string(), ts, replica: name.to_string(), stream: stream_name, line: text });
        }
        if saved_at.elapsed() > Duration::from_secs(5) {
            let _ = app.db.set_state(&pos_key, &newest.to_string());
            saved_at = std::time::Instant::now();
        }
    }
    let _ = app.db.set_state(&pos_key, &newest.to_string());
}

fn masker_fn(app: &App, service_id: &str) -> Masker {
    match crate::store::service(app, service_id) {
        Ok(Some(s)) => Masker::for_service(app, &s),
        _ => Masker::new(std::iter::empty()),
    }
}

/// Docker prefixes lines with an RFC 3339 timestamp when asked to.
fn split_timestamp(raw: &str) -> (i64, &str) {
    match raw.split_once(' ') {
        Some((ts, rest)) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(t) => (t.timestamp_millis(), rest),
            Err(_) => (util::now_ms(), raw),
        },
        None => (util::now_ms(), raw),
    }
}

pub struct Query {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub q: Option<String>,
    pub replica: Option<String>,
    pub limit: usize,
}

/// The newest `limit` matching lines, oldest first.
pub fn search(app: &App, service_id: &str, q: &Query) -> Result<Vec<LogLine>> {
    let dir = service_dir(app, service_id);
    let mut days: Vec<PathBuf> = std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    days.sort();
    let needle = q.q.as_ref().map(|s| s.to_lowercase());
    let mut out: VecDeque<LogLine> = VecDeque::new();
    for day in days.iter().rev() {
        let text = std::fs::read_to_string(day).unwrap_or_default();
        let mut matched: Vec<LogLine> = vec![];
        for l in text.lines() {
            let mut parts = l.splitn(4, '\t');
            let (Some(ts), Some(replica), Some(stream), Some(line)) = (parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
            let Ok(ts) = ts.parse::<i64>() else { continue };
            if q.since.is_some_and(|s| ts < s) || q.until.is_some_and(|u| ts > u) {
                continue;
            }
            if q.replica.as_ref().is_some_and(|r| r != replica) {
                continue;
            }
            if needle.as_ref().is_some_and(|n| !line.to_lowercase().contains(n)) {
                continue;
            }
            matched.push(LogLine {
                service_id: service_id.into(),
                ts,
                replica: replica.into(),
                stream: if stream == "stderr" { "stderr" } else { "stdout" },
                line: line.into(),
            });
        }
        for l in matched.into_iter().rev() {
            if out.len() >= q.limit {
                break;
            }
            out.push_front(l);
        }
        if out.len() >= q.limit || q.since.is_some_and(|s| day_start(day) < s) {
            break;
        }
    }
    let mut v: Vec<LogLine> = out.into_iter().collect();
    v.sort_by_key(|l| l.ts);
    Ok(v)
}

fn day_start(path: &Path) -> i64 {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc().timestamp_millis())
        .unwrap_or(0)
}

pub fn usage(app: &App) -> (u64, Vec<(String, u64)>) {
    let mut total = 0;
    let mut per = vec![];
    for e in std::fs::read_dir(app.paths.logs()).map(|r| r.flatten().collect::<Vec<_>>()).unwrap_or_default() {
        let size: u64 = std::fs::read_dir(e.path()).map(|r| r.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0);
        total += size;
        per.push((e.file_name().to_string_lossy().to_string(), size));
    }
    per.sort_by_key(|p| std::cmp::Reverse(p.1));
    (total, per)
}

/// Optional cleanup (off by default): delete the oldest day files beyond the size or age cap.
pub fn cleanup(app: &App) -> Result<usize> {
    let s = app.settings().log_cleanup;
    if !s.enabled {
        return Ok(0);
    }
    let mut files: Vec<(PathBuf, i64, u64)> = vec![];
    for dir in std::fs::read_dir(app.paths.logs())?.flatten() {
        for f in std::fs::read_dir(dir.path()).map(|r| r.flatten().collect::<Vec<_>>()).unwrap_or_default() {
            let p = f.path();
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            files.push((p.clone(), day_start(&p), size));
        }
    }
    files.sort_by_key(|f| f.1);
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    let cap = (s.max_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let oldest_kept = s.max_days.map(|d| util::now_ms() - d as i64 * 86_400_000);
    let mut removed = 0;
    for (path, start, size) in files {
        let too_old = oldest_kept.is_some_and(|o| start + 86_400_000 < o);
        if !too_old && total <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed += 1;
        }
    }
    if removed > 0 {
        tracing::info!("log cleanup removed {removed} old files");
    }
    Ok(removed)
}

/// Tugport's own log: JSON lines in data/logs/_tugport plus the last lines in memory.
pub struct SelfLog {
    pub recent: Mutex<VecDeque<String>>,
    file: Mutex<Option<(String, std::fs::File)>>,
    dir: PathBuf,
}

impl SelfLog {
    pub fn new(dir: PathBuf) -> Arc<Self> {
        let _ = std::fs::create_dir_all(&dir);
        Arc::new(SelfLog { recent: Mutex::new(VecDeque::with_capacity(2000)), file: Mutex::new(None), dir })
    }

    fn write_line(&self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        {
            let mut r = crate::util::lock(&self.recent);
            if r.len() >= 2000 {
                r.pop_front();
            }
            r.push_back(line.to_string());
        }
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut f = crate::util::lock(&self.file);
        if f.as_ref().is_none_or(|(d, _)| *d != day) {
            *f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(format!("{day}.log"))).ok().map(|file| (day, file));
        }
        if let Some((_, file)) = f.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }
}

pub struct SelfLogWriter(pub Arc<SelfLog>);

impl Write for SelfLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for line in String::from_utf8_lossy(buf).lines() {
            self.0.write_line(line);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SelfLogHandle {
    type Writer = SelfLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        SelfLogWriter(self.0.clone())
    }
}

#[derive(Clone)]
pub struct SelfLogHandle(pub Arc<SelfLog>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_docker_timestamps() {
        let (ts, text) = split_timestamp("2026-10-22T21:00:33.807623456Z hello world");
        assert_eq!(text, "hello world");
        assert_eq!(ts, chrono::DateTime::parse_from_rfc3339("2026-10-22T21:00:33.807Z").unwrap().timestamp_millis());
    }
}
