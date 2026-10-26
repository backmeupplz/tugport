//! Backups with restic (run from its official image). Databases are dumped through the
//! container's init and streamed in, volumes are read directly, and Tugport's own state
//! is snapshotted. Repositories are encrypted with a password derived from the master key.

use crate::app::{App, AppState};
use crate::model::Engine;
use crate::{databases, runtime, store, util};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const RESTIC: &str = "restic/restic:0.19.1";

/// S3 storage classes a target can keep its backup data in; Standard when none is set.
pub const STORAGE_CLASSES: [&str; 4] = ["STANDARD", "STANDARD_IA", "GLACIER_IR", "DEEP_ARCHIVE"];
const DEEP_ARCHIVE: &str = "DEEP_ARCHIVE";

pub struct Target {
    id: String,
    kind: String,
    config: Value,
}

impl Target {
    /// The S3 storage class backup data goes to, None for Standard (and local targets).
    pub fn storage_class(&self) -> Option<&str> {
        self.config.get("storage_class").and_then(|v| v.as_str()).filter(|c| self.kind == "s3" && *c != "STANDARD")
    }
}

pub struct Job {
    pub id: String,
    pub service_id: Option<String>,
    target_id: String,
    pub kind: String,
    volume: Option<String>,
    keep_last: i64,
}

pub fn target(app: &App, id: &str) -> Result<Target> {
    let (kind, config): (String, Vec<u8>) =
        app.db.row("SELECT kind, config FROM backup_targets WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?.ok_or_else(|| anyhow!("backup target {id} not found"))?;
    let config = serde_json::from_str(&app.keys.open_str(&format!("backup-target:{id}"), &config)?)?;
    Ok(Target { id: id.into(), kind, config })
}

pub fn job(app: &App, id: &str) -> Result<Job> {
    app.db
        .row("SELECT id, service_id, target_id, kind, volume, keep_last FROM backup_jobs WHERE id = ?1", [id], |r| {
            Ok(Job { id: r.get(0)?, service_id: r.get(1)?, target_id: r.get(2)?, kind: r.get(3)?, volume: r.get(4)?, keep_last: r.get(5)? })
        })?
        .ok_or_else(|| anyhow!("backup job {id} not found"))
}

/// Restic settings for a target, written to a memory-only env file for `docker run --env-file`.
struct Repo {
    env_file: PathBuf,
    mounts: Vec<String>,
    class: Option<String>,
}

impl Repo {
    fn archived(&self) -> bool {
        self.class.as_deref() == Some(DEEP_ARCHIVE)
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.env_file);
    }
}

fn repo(app: &App, t: &Target) -> Result<Repo> {
    let field = |k: &str| t.config.get(k).and_then(|v| v.as_str()).map(String::from);
    let mut env = vec![("RESTIC_PASSWORD".to_string(), app.keys.derive(&format!("restic:{}", t.id))?)];
    let mut mounts = vec![];
    match t.kind.as_str() {
        "s3" => {
            let endpoint = field("endpoint").unwrap_or_else(|| "s3.amazonaws.com".into());
            let endpoint = endpoint.trim_start_matches("https://").trim_end_matches('/');
            let bucket = field("bucket").ok_or_else(|| anyhow!("the S3 target has no bucket"))?;
            let prefix = field("prefix").unwrap_or_default();
            let path = [bucket.as_str(), prefix.trim_matches('/'), "tugport"].iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join("/");
            env.push(("RESTIC_REPOSITORY".into(), format!("s3:https://{endpoint}/{path}")));
            env.push(("AWS_ACCESS_KEY_ID".into(), field("access_key").unwrap_or_default()));
            env.push(("AWS_SECRET_ACCESS_KEY".into(), field("secret_key").unwrap_or_default()));
            if let Some(region) = field("region") {
                env.push(("AWS_DEFAULT_REGION".into(), region));
            }
        }
        "local" => {
            let path = field("path").ok_or_else(|| anyhow!("the local target has no path"))?;
            std::fs::create_dir_all(&path)?;
            mounts.push(format!("{path}:/repo"));
            env.push(("RESTIC_REPOSITORY".into(), "/repo".into()));
        }
        other => bail!("unknown target kind {other}"),
    }
    // Made first, so a failed write is cleaned up too.
    let repo = Repo { env_file: app.paths.run.join(format!("restic-{}.env", util::id())), mounts, class: t.storage_class().map(String::from) };
    let lines: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    util::write_private(&repo.env_file, lines.as_bytes())?;
    Ok(repo)
}

fn restic_cmd(repo: &Repo, extra_mounts: &[String], args: &[&str]) -> Command {
    restic_on(Command::new("docker"), repo, extra_mounts, args)
}

/// Restic run by `docker` aimed at some server (volumes are read where they live).
fn restic_on(mut c: Command, repo: &Repo, extra_mounts: &[String], args: &[&str]) -> Command {
    // With --json restic reports progress 60 times a second unless told otherwise.
    c.args(["run", "--rm", "-i", "--hostname", "tugport", "-e", "RESTIC_PROGRESS_FPS=1", "--env-file"]).arg(&repo.env_file);
    for m in repo.mounts.iter().chain(extra_mounts) {
        c.arg("-v").arg(m);
    }
    c.args(restic_args(repo.class.as_deref(), args)).kill_on_drop(true);
    c
}

/// What follows the mounts in `docker run`: the image and restic's arguments, with the target's
/// storage class. In Deep Archive restic keeps only file data (its config, keys, snapshots, index
/// and folder trees stay in Standard, so backups work as usual), and so:
/// - retention repacks only the Standard tree packs with `--repack-cacheable-only`, never reading
///   archived data (`--max-unused unlimited` would not do: prune still repacks small data packs);
/// - restores ask S3 to thaw the archived packs they need and wait for them, up to 48 hours.
fn restic_args(class: Option<&str>, args: &[&str]) -> Vec<String> {
    let deep = class == Some(DEEP_ARCHIVE);
    let thaw = deep && args.first() == Some(&"restore");
    let mut out = vec![];
    if thaw {
        out.extend(["-e".to_string(), "RESTIC_FEATURES=s3-restore".into()]);
    }
    out.push(RESTIC.to_string());
    if let Some(class) = class {
        out.extend(["-o".to_string(), format!("s3.storage-class={class}")]);
    }
    if thaw {
        for o in ["s3.enable-restore=true", "s3.restore-days=7", "s3.restore-timeout=48h"] {
            out.extend(["-o".to_string(), o.into()]);
        }
    }
    out.extend(args.iter().map(|a| a.to_string()));
    if deep && args.first() == Some(&"forget") {
        out.push("--repack-cacheable-only".into());
    }
    out
}

/// Deep Archive: have S3 thaw what a restore of `include` needs and wait for it (up to 48 hours) by
/// restoring it into the throwaway restic container. It then stays readable for 7 days, so the real
/// restore after it (and any downtime around that) does not wait.
async fn thaw(cli: Command, repo: &Repo, snapshot: &str, include: &str, log: &RunLog) -> Result<()> {
    if repo.archived() {
        log.line("Waiting for S3 to thaw the backup from Glacier Deep Archive (up to 48 hours)…");
        log.run(restic_on(cli, repo, &[], &["restore", "--json", snapshot, "--target", "/thaw", "--include", include])).await.context("thawing the backup from Deep Archive")?;
    }
    Ok(())
}

/// Whether a job's backups are in Deep Archive, where a restore first waits up to 48 hours for S3.
pub fn archived(app: &App, j: &Job) -> bool {
    target(app, &j.target_id).is_ok_and(|t| t.storage_class() == Some(DEEP_ARCHIVE))
}

async fn ensure_repo(repo: &Repo, log: &RunLog) -> Result<()> {
    let mut check = restic_cmd(repo, &[], &["cat", "config"]);
    check.stdout(Stdio::null()).stderr(Stdio::null());
    if check.status().await?.success() {
        return Ok(());
    }
    log.line("Creating the backup repository…");
    crate::proc::output(restic_cmd(repo, &[], &["init"])).await.context("initialising the backup repository")?;
    Ok(())
}

/// The server a job's volume lives on: its service's server, else this one.
fn volume_node(app: &App, j: &Job) -> String {
    j.service_id.as_ref().and_then(|sid| app.db.state::<String>(&format!("pin:{sid}"))).unwrap_or_else(|| "local".into())
}

/// `docker exec` of a shell command in a container, through Tugport's init (so with the
/// container's environment). `stdin` keeps standard input open for a pipe.
pub fn exec_cli(app: &App, node: &str, container: &str, stdin: bool, script: &str) -> Result<Command> {
    let mut c = crate::nodes::docker_cli(app, node)?;
    c.arg("exec");
    if stdin {
        c.arg("-i");
    }
    c.args([container, crate::secrets::INIT_PATH, "exec", "--", "sh", "-c", script]);
    Ok(c)
}

/// Run `producer`'s stdout into `consumer`'s stdin. Each line the consumer writes goes to
/// `line`, with whether it came from its stderr.
pub async fn pipe(mut producer: Command, mut consumer: Command, line: &(dyn Fn(&str, bool) + Sync)) -> Result<()> {
    producer.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
    consumer.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut p = producer.spawn()?;
    let mut c = consumer.spawn()?;
    let mut from = p.stdout.take().ok_or_else(|| anyhow!("no producer stdout"))?;
    let mut to = c.stdin.take().ok_or_else(|| anyhow!("no consumer stdin"))?;
    let mut out = BufReader::new(c.stdout.take().ok_or_else(|| anyhow!("no consumer stdout"))?).lines();
    let mut err = BufReader::new(c.stderr.take().ok_or_else(|| anyhow!("no consumer stderr"))?).lines();
    let copy = async move {
        let n = tokio::io::copy(&mut from, &mut to).await;
        drop(to);
        n
    };
    let mut tail = VecDeque::new();
    let read = async {
        let (mut out_done, mut err_done) = (false, false);
        while !(out_done && err_done) {
            tokio::select! {
                l = out.next_line(), if !out_done => match l {
                    Ok(Some(l)) => line(&l, false),
                    _ => out_done = true,
                },
                l = err.next_line(), if !err_done => match l {
                    Ok(Some(l)) => {
                        line(&l, true);
                        tail.push_back(l);
                        if tail.len() > 3 {
                            tail.pop_front();
                        }
                    }
                    _ => err_done = true,
                },
            }
        }
        c.wait().await
    };
    // All at once: a consumer that writes a lot (restic's progress) must not stall the copy.
    let (p_out, c_status, copied) = tokio::join!(p.wait_with_output(), read, copy);
    let p_out = p_out?;
    if !p_out.status.success() {
        bail!("the dump failed: {}", String::from_utf8_lossy(&p_out.stderr).trim());
    }
    if !c_status?.success() {
        bail!("restic failed: {}", Vec::from(tail).join("\n").trim());
    }
    if copied? == 0 {
        bail!("the dump was empty");
    }
    Ok(())
}

/// The snapshot and size in restic's summary message.
fn summary(v: &Value) -> (Option<String>, Option<i64>) {
    let size = v["total_bytes_processed"].as_i64().or(v["data_added"].as_i64());
    (v["snapshot_id"].as_str().map(String::from), size)
}

/// Runs left mid-way by a crash or restart: mark them failed.
pub fn recover_interrupted(app: &App) -> Result<()> {
    app.db.exec("UPDATE backup_runs SET status = 'failed', error = 'Tugport restarted during this run', finished_at = ?1 WHERE status = 'running'", [util::now()])?;
    Ok(())
}

/// The latest progress of the runs going on now, by run id.
static PROGRESS: Mutex<BTreeMap<String, Value>> = Mutex::new(BTreeMap::new());
/// Lines a run's log keeps; restic can warn once per file.
const LOG_LINES: usize = 2000;

/// How far a running backup or restore is: `{percent, bytes_done, total_bytes, files_done, total_files, seconds_remaining}`.
pub fn progress(run_id: &str) -> Option<Value> {
    util::lock(&PROGRESS).get(run_id).cloned()
}

pub fn log_path(app: &App, run_id: &str) -> PathBuf {
    app.paths.backup_logs().join(format!("{run_id}.log"))
}

#[derive(Default)]
struct LogState {
    file: Option<std::fs::File>,
    lines: usize,
    /// When progress last went into the log as a line.
    said: Option<std::time::Instant>,
    /// The last text lines of the command running now, for its error.
    tail: VecDeque<String>,
}

/// A run's log: its steps and restic's warnings and errors go to a file and to watchers, while
/// restic's progress stays in memory as the run's latest, written down as a line once a minute.
pub struct RunLog {
    app: AppState,
    pub run_id: String,
    project_id: Option<String>,
    service_id: Option<String>,
    verb: &'static str,
    state: Mutex<LogState>,
}

impl RunLog {
    pub fn line(&self, text: &str) {
        // One line in the file each (errors can span several).
        let mut text = text.trim_end().replace(['\n', '\r'], " ");
        if text.len() > 2000 {
            text = format!("{}…", text.chars().take(2000).collect::<String>());
        }
        let ts = util::now_ms();
        {
            let mut st = util::lock(&self.state);
            st.lines += 1;
            if st.lines > LOG_LINES {
                return;
            }
            if st.lines == LOG_LINES {
                text = "The log is cut here; it keeps the first lines only.".into();
            }
            if let Some(f) = st.file.as_mut() {
                let _ = writeln!(f, "{ts}\t{text}");
            }
        }
        self.emit("backup.log", json!({ "run_id": self.run_id, "ts": ts, "line": text }));
    }

    fn emit(&self, kind: &str, data: Value) {
        self.app.emit(kind, self.project_id.as_deref(), self.service_id.as_deref(), data);
    }

    /// A line of restic's output (with `--json`, else plain text); returns its summary message.
    fn restic(&self, raw: &str) -> Option<Value> {
        let Ok(v) = serde_json::from_str::<Value>(raw) else {
            self.text(raw);
            return None;
        };
        match v["message_type"].as_str() {
            Some("status") => self.progress(&v),
            Some("summary") => return Some(v),
            Some("exit_error") => self.text(v["message"].as_str().unwrap_or(raw)),
            Some("error") => {
                let item = v["item"].as_str().map(|i| format!("{i}: ")).unwrap_or_default();
                self.text(&format!("{item}{}", v["error"]["message"].as_str().unwrap_or(raw)));
            }
            _ => {}
        }
        None
    }

    /// Text output, kept for the error if the command fails.
    fn text(&self, text: &str) {
        // Restic's messages may span lines; the log keeps one per line.
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            {
                let mut st = util::lock(&self.state);
                st.tail.push_back(line.to_string());
                if st.tail.len() > 3 {
                    st.tail.pop_front();
                }
            }
            self.line(line);
        }
    }

    fn progress(&self, v: &Value) {
        let done = v["bytes_done"].as_u64().or(v["bytes_restored"].as_u64()).unwrap_or(0);
        let total = v["total_bytes"].as_u64().unwrap_or(0);
        let percent = (v["percent_done"].as_f64().unwrap_or(0.0) * 100.0).clamp(0.0, 100.0);
        let p = json!({
            "percent": percent,
            "bytes_done": done,
            "total_bytes": total,
            "files_done": v["files_done"].as_u64().or(v["files_restored"].as_u64()).unwrap_or(0),
            "total_files": v["total_files"].as_u64().unwrap_or(0),
            "seconds_remaining": v["seconds_remaining"],
        });
        util::lock(&PROGRESS).insert(self.run_id.clone(), p.clone());
        self.emit("backup.progress", json!({ "run_id": self.run_id, "progress": p }));
        let due = {
            let mut st = util::lock(&self.state);
            let due = st.said.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
            if due {
                st.said = Some(std::time::Instant::now());
            }
            due
        };
        if due {
            let verb = self.verb;
            self.line(&if total > 0 { format!("{verb} {} of {} ({percent:.0}%)", human(done), human(total)) } else { format!("{verb} {} so far", human(done)) });
        }
    }

    /// Run a restic command, its output going to the log; returns its summary message.
    async fn run(&self, cmd: Command) -> Result<Option<Value>> {
        util::lock(&self.state).tail.clear();
        let summary = Mutex::new(None);
        let res = crate::proc::run_streamed(cmd, &|l| {
            if let Some(s) = self.restic(l) {
                *util::lock(&summary) = Some(s);
            }
        })
        .await;
        if let Err(e) = res {
            let tail = Vec::from(std::mem::take(&mut util::lock(&self.state).tail));
            if tail.is_empty() {
                return Err(e);
            }
            bail!("restic failed: {}", tail.join(" | "));
        }
        Ok(summary.into_inner().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for RunLog {
    fn drop(&mut self) {
        util::lock(&PROGRESS).remove(&self.run_id);
    }
}

/// 1.2 GB.
fn human(n: u64) -> String {
    let (mut v, mut unit) = (n as f64, 0);
    while v >= 1024.0 && unit < 4 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{v:.1} {}", ["B", "KB", "MB", "GB", "TB"][unit]) }
}

/// Record a run (backup, restore or test-restore) as started, with its log.
fn start_run(app: &AppState, j: &Job, kind: &str, snapshot: Option<&str>) -> Result<RunLog> {
    let id = util::id();
    app.db.exec(
        "INSERT INTO backup_runs (id, job_id, kind, status, snapshot, started_at) VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
        params![id, j.id, kind, snapshot, util::now()],
    )?;
    let file = std::fs::OpenOptions::new().create(true).append(true).open(log_path(app, &id)).ok();
    let log = RunLog {
        app: app.clone(),
        project_id: j.service_id.as_deref().and_then(|sid| store::service(app, sid).ok().flatten()).map(|s| s.project_id),
        service_id: j.service_id.clone(),
        verb: if kind == "backup" { "Backed up" } else { "Restored" },
        state: Mutex::new(LogState { file, ..Default::default() }),
        run_id: id,
    };
    log.emit("backup", json!({ "run_id": log.run_id, "status": "running" }));
    Ok(log)
}

/// Record how a run ended; a restore keeps the snapshot it started from.
fn end_run(app: &App, log: &RunLog, error: Option<&str>, snapshot: Option<&str>, size: Option<i64>) -> Result<()> {
    let status = if error.is_some() { "failed" } else { "ok" };
    if let Some(e) = error {
        log.line(&format!("Failed: {e}"));
    }
    app.db.exec(
        "UPDATE backup_runs SET status = ?1, error = ?2, snapshot = COALESCE(?3, snapshot), size_bytes = ?4, finished_at = ?5 WHERE id = ?6",
        params![status, error, snapshot, size, util::now(), log.run_id],
    )?;
    util::lock(&PROGRESS).remove(&log.run_id);
    log.emit("backup", json!({ "run_id": log.run_id, "status": status, "ok": error.is_none() }));
    Ok(())
}

/// Run a backup job now. Returns the run id.
pub async fn run_job(app: &AppState, job_id: &str) -> Result<String> {
    let j = job(app, job_id)?;
    let log = start_run(app, &j, "backup", None)?;
    let result = backup(app, &j, &log).await;
    match &result {
        Ok((snapshot, size)) => {
            let short = snapshot.as_deref().map(|s| format!(" as snapshot {}", s.get(..8).unwrap_or(s))).unwrap_or_default();
            log.line(&format!("Done: {} backed up{short}.", size.map_or("nothing".into(), |n| human(n.max(0) as u64))));
            end_run(app, &log, None, snapshot.as_deref(), *size)?
        }
        Err(e) => {
            let msg = format!("{e:#}");
            end_run(app, &log, Some(&msg), None, None)?;
            let s = j.service_id.as_deref().and_then(|id| store::service(app, id).ok().flatten());
            let what = s.as_ref().map(|s| format!("{}/{}", s.project, s.name)).unwrap_or_else(|| "Tugport".into());
            crate::notify::event(app, "backup.failed", &format!("Backup of {what} failed"), &msg, s.as_ref()).await;
        }
    }
    result.map(|_| log.run_id.clone())
}

/// A database job's service and engine.
fn database(app: &App, j: &Job) -> Result<(store::Service, Engine)> {
    let sid = j.service_id.as_deref().ok_or_else(|| anyhow!("database backups need a service"))?;
    let s = store::service(app, sid)?.ok_or_else(|| anyhow!("the service was deleted"))?;
    let engine = s.spec.database.as_ref().map(|d| d.engine).ok_or_else(|| anyhow!("{} is not a database", s.name))?;
    Ok((s, engine))
}

async fn backup(app: &App, j: &Job, log: &RunLog) -> Result<(Option<String>, Option<i64>)> {
    let t = target(app, &j.target_id)?;
    let repo = repo(app, &t)?;
    ensure_repo(&repo, log).await?;
    let job_tag = format!("job:{}", j.id);
    let out = match j.kind.as_str() {
        "database" => {
            let (s, engine) = database(app, j)?;
            let ctr = runtime::running_replica(app, &s).await?;
            let dump = exec_cli(app, &ctr.node, &ctr.id, false, databases::preset(engine).dump)?;
            let filename = format!("{}/{}", s.name, databases::preset(engine).dump_file);
            let restic = restic_cmd(&repo, &[], &["backup", "--json", "--stdin", "--stdin-filename", &filename, "--tag", "tugport", "--tag", &job_tag]);
            log.line(&format!("Dumping {} into the backup…", s.name));
            let summary = Mutex::new(None);
            pipe(dump, restic, &|l, _| {
                if let Some(s) = log.restic(l) {
                    *util::lock(&summary) = Some(s);
                }
            })
            .await?;
            summary.into_inner().unwrap_or_else(|e| e.into_inner())
        }
        "volume" => {
            let vol = volume_name(j)?;
            let mount = format!("{vol}:/data/{vol}:ro");
            let cli = crate::nodes::docker_cli(app, &volume_node(app, j))?;
            log.line(&format!("Backing up volume {vol}…"));
            log.run(restic_on(cli, &repo, &[mount], &["backup", "--json", &format!("/data/{vol}"), "--tag", "tugport", "--tag", &job_tag])).await?
        }
        "state" => {
            let dir = app.paths.data.join("backup-staging");
            std::fs::create_dir_all(&dir)?;
            log.line("Copying Tugport's database…");
            app.db.snapshot(&dir.join("tugport.db"))?;
            let mount = format!("{}:/data/tugport:ro", dir.display());
            log.line("Backing up Tugport's state…");
            let out = log.run(restic_cmd(&repo, &[mount], &["backup", "--json", "/data/tugport", "--tag", "tugport", "--tag", &job_tag])).await;
            let _ = std::fs::remove_file(dir.join("tugport.db"));
            out?
        }
        other => bail!("unknown backup kind {other}"),
    };
    let (snapshot, size) = out.as_ref().map(summary).unwrap_or_default();
    let keep = j.keep_last.max(1);
    log.line(&format!("Applying retention (keep the last {keep})…"));
    crate::proc::output(restic_cmd(&repo, &[], &["forget", "--tag", &job_tag, "--keep-last", &keep.to_string(), "--prune"])).await.context("applying retention")?;
    Ok((snapshot, size))
}

/// Restore a run in place, or (test) into a throwaway copy that is checked and removed.
pub async fn restore(app: &AppState, run_id: &str, test: bool) -> Result<String> {
    let (job_id, snapshot): (String, Option<String>) =
        app.db.row("SELECT job_id, snapshot FROM backup_runs WHERE id = ?1", [run_id], |r| Ok((r.get(0)?, r.get(1)?)))?.ok_or_else(|| anyhow!("backup run {run_id} not found"))?;
    let snapshot = snapshot.ok_or_else(|| anyhow!("this run has no snapshot"))?;
    let j = job(app, &job_id)?;
    let t = target(app, &j.target_id)?;
    let repo = repo(app, &t)?;
    let log = start_run(app, &j, if test { "test-restore" } else { "restore" }, Some(&snapshot))?;
    log.line(&format!("{} snapshot {}…", if test { "Test-restoring" } else { "Restoring" }, snapshot.get(..8).unwrap_or(&snapshot)));
    let result = match j.kind.as_str() {
        // Reading archived data takes S3 up to 48 hours, so a test checks only what is in Standard.
        _ if test && repo.archived() => verify_snapshot(&repo, &snapshot, &log).await,
        "database" => restore_database(app, &j, &repo, &snapshot, test, &log).await,
        "volume" if !test => restore_volume(app, &j, &repo, &snapshot, &log).await,
        "volume" => verify_snapshot(&repo, &snapshot, &log).await,
        "state" => restore_state(app, &repo, &snapshot, &log).await,
        other => Err(anyhow!("unknown backup kind {other}")),
    };
    if let Ok(msg) = &result {
        log.line(msg);
    }
    end_run(app, &log, result.as_ref().err().map(|e| format!("{e:#}")).as_deref(), None, None)?;
    result
}

async fn verify_snapshot(repo: &Repo, snapshot: &str, log: &RunLog) -> Result<String> {
    // Without --read-data, check reads only the index and folder trees, which stay in Standard.
    let check: &[&str] = if repo.archived() { &["check"] } else { &["check", "--read-data-subset=5%"] };
    log.line("Checking the repository…");
    log.run(restic_cmd(repo, &[], check)).await?;
    log.line("Listing the snapshot…");
    crate::proc::output(restic_cmd(repo, &[], &["ls", snapshot])).await?;
    if repo.archived() {
        return Ok("The snapshot is listed and the repository's index and folder trees passed their integrity check. The data itself is in S3 Glacier Deep Archive, so it was not read; a real restore thaws it first, which takes up to 48 hours.".into());
    }
    Ok("The snapshot is readable and the repository passed its integrity check.".into())
}

async fn restore_database(app: &AppState, j: &Job, repo: &Repo, snapshot: &str, test: bool, log: &RunLog) -> Result<String> {
    let (s, engine) = database(app, j)?;
    if engine == Engine::Redis && !test {
        bail!("restoring Redis replaces its data file; stop the service and use the volume backup instead");
    }
    let db = databases::preset(engine);
    let path = format!("/{}/{}", s.name, db.dump_file);
    // `restic dump` cannot thaw archived data itself.
    thaw(Command::new("docker"), repo, snapshot, &path, log).await?;
    let (node, container, scratch_name) = if test {
        log.line(&format!("Starting a temporary copy of {}…", s.name));
        let (node, id, name) = scratch_database(app, &s).await?;
        (node, id, Some(name))
    } else {
        let r = runtime::running_replica(app, &s).await?;
        (r.node, r.id, None)
    };
    // The temporary copy goes whatever happens from here on.
    let result = async {
        let load = exec_cli(app, &node, &container, true, db.restore)?;
        log.line(&format!("Loading the dump into {}…", if test { "the temporary copy" } else { s.name.as_str() }));
        // The database's errors and warnings; what it reports for every statement would drown them.
        pipe(restic_cmd(repo, &[], &["dump", snapshot, &path]), load, &|l, stderr| {
            if stderr {
                log.line(l);
            }
        })
        .await?;
        if !test {
            return Ok("Restored in place.".to_string());
        }
        log.line("Counting what the copy holds…");
        let count = crate::proc::output(exec_cli(app, &node, &container, false, db.count)?).await?;
        Ok(format!("Test restore succeeded. The restored copy reports {} tables/collections/keys.", count.trim()))
    }
    .await;
    if let Some(name) = scratch_name
        && let Err(e) = runtime::remove(app, &node, &container, &name).await
    {
        tracing::warn!("removing the test restore {name}: {e:#}");
    }
    result
}

/// A temporary copy of a database service (same image and credentials, empty data) on
/// the database's server. It gets its own DNS name so nothing can mistake it for the real one.
async fn scratch_database(app: &AppState, s: &store::Service) -> Result<(String, String, String)> {
    let mut copy = s.clone();
    copy.name = format!("{}-restoretest", s.name);
    copy.spec.mounts.retain(|m| !matches!(m, crate::model::Mount::Volume { .. }));
    copy.spec.ports.clear();
    copy.spec.aliases.clear();
    copy.spec.networks.clear();
    let crate::model::Source::Image { image } = &s.spec.source else { bail!("databases run from an image") };
    let network = runtime::ensure_network(app, &s.project).await?;
    let img = runtime::image_info(app, image).await?;
    let name = runtime::container_name(&copy, "restoretest", 1);
    let node = crate::nodes::home(app, s)?;
    crate::nodes::ship_image(app, &node, s, image, &|_| {}).await?;
    let id = runtime::create_replica(app, &node, &copy, "restoretest", 1, image, &img, &network, false).await?;
    let started = async {
        runtime::start(app, &node.id, &id).await?;
        runtime::wait_healthy(app, &copy, &node.id, &id, &network, Duration::from_secs(120), img.has_healthcheck, &|_| {}).await.context("the temporary database did not start")
    }
    .await;
    if let Err(e) = started {
        let _ = runtime::remove(app, &node.id, &id, &name).await;
        return Err(e);
    }
    Ok((node.id.clone(), id, name))
}

/// A job's volume, refused when it is not a plain volume name: in `docker run -v` a path would
/// be a folder of the server.
fn volume_name(j: &Job) -> Result<&str> {
    let vol = j.volume.as_deref().ok_or_else(|| anyhow!("volume backups need a volume"))?;
    if vol.is_empty() || vol.starts_with(['.', '-']) || vol.contains(['/', ':', ',']) {
        bail!("{vol} is not a volume name");
    }
    Ok(vol)
}

async fn restore_volume(app: &AppState, j: &Job, repo: &Repo, snapshot: &str, log: &RunLog) -> Result<String> {
    let vol = volume_name(j)?;
    // Before anything stops, so the service keeps running while S3 thaws the data.
    thaw(crate::nodes::docker_cli(app, &volume_node(app, j))?, repo, snapshot, &format!("/data/{vol}"), log).await?;
    let users: Vec<runtime::Replica> = match &j.service_id {
        Some(sid) => runtime::replicas(app, sid).await?.into_iter().filter(|r| r.state == "running").collect(),
        None => vec![],
    };
    // Whatever was stopped starts again, also when stopping another one or the restore fails.
    let mut stopped = vec![];
    let res = async {
        for r in &users {
            log.line(&format!("Stopping {}…", r.name));
            runtime::stop(app, &r.node, &r.id, 30).await?;
            stopped.push(r);
        }
        let mount = format!("{vol}:/data/{vol}");
        let cli = crate::nodes::docker_cli(app, &volume_node(app, j))?;
        log.line(&format!("Restoring volume {vol}…"));
        log.run(restic_on(cli, repo, &[mount], &["restore", "--json", snapshot, "--target", "/", "--include", &format!("/data/{vol}")])).await
    }
    .await;
    for r in stopped {
        log.line(&format!("Starting {} again…", r.name));
        if let Err(e) = runtime::start(app, &r.node, &r.id).await {
            tracing::warn!("starting {} again after restoring {vol}: {e:#}", r.name);
        }
    }
    // Restarted containers may have new addresses, also when the restore failed.
    crate::proxy::reload(app);
    res?;
    Ok(format!("Restored volume {vol}."))
}

async fn restore_state(app: &App, repo: &Repo, snapshot: &str, log: &RunLog) -> Result<String> {
    let dir = app.paths.data.join("restored");
    std::fs::create_dir_all(&dir)?;
    let mount = format!("{}:/out", dir.display());
    log.run(restic_cmd(repo, &[mount], &["restore", "--json", snapshot, "--target", "/out"])).await?;
    Ok(format!(
        "Tugport's state was restored to {}/data/tugport/tugport.db. Stop Tugport and replace {} with it to roll back.",
        dir.display(),
        app.paths.db().display()
    ))
}

/// Start due jobs. One run per job at a time.
pub async fn scheduler(app: AppState) {
    let running: Arc<Mutex<HashSet<String>>> = Default::default();
    crate::util::every(30, "backup scheduler", || start_due(app.clone(), running.clone())).await
}

async fn start_due(app: AppState, running: Arc<Mutex<HashSet<String>>>) -> Result<()> {
    // Each job counts from its last run, or from when it was made.
    let jobs: Vec<(String, String, i64)> = app.db.rows(
        "SELECT j.id, j.schedule, MAX(j.created_at, COALESCE((SELECT MAX(r.started_at) FROM backup_runs r WHERE r.job_id = j.id AND r.kind = 'backup'), 0))
         FROM backup_jobs j WHERE j.enabled = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let now = chrono::Utc::now();
    for (id, schedule, last) in jobs {
        let Some(cron) = cron(&schedule) else { continue };
        let from = chrono::DateTime::from_timestamp(last, 0).unwrap_or(now);
        let Ok(next) = cron.find_next_occurrence(&from, false) else { continue };
        if next > now || !util::lock(&running).insert(id.clone()) {
            continue;
        }
        let (app, running) = (app.clone(), running.clone());
        tokio::spawn(async move {
            if let Err(e) = run_job(&app, &id).await {
                tracing::warn!("backup job {id} failed: {e:#}");
            }
            util::lock(&running).remove(&id);
        });
    }
    Ok(())
}

fn cron(s: &str) -> Option<croner::Cron> {
    s.parse().ok()
}

pub fn valid_schedule(s: &str) -> bool {
    cron(s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_restic_summary() {
        assert!(valid_schedule("0 3 * * *"));
        assert!(!valid_schedule("every day"));
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1288490189), "1.2 GB");
    }

    /// Restic's JSON output: progress is kept but not logged line by line, errors and text are logged.
    #[tokio::test]
    async fn run_log_reads_restic() {
        let dir = std::env::temp_dir().join(format!("tugport-test-{}", util::id()));
        let paths = crate::paths::Paths { etc: dir.join("etc"), data: dir.join("data"), run: dir.join("run") };
        std::fs::create_dir_all(paths.backup_logs()).unwrap();
        let db = crate::db::Db::open(&paths.db()).unwrap();
        let selflog = crate::logs::SelfLog::new(dir.join("self"));
        // No Docker needed (CI has none).
        let docker = bollard::Docker::connect_with_custom_transport(
            |_: bollard::BollardRequest| async { Err(bollard::errors::Error::RequestTimeoutError) },
            Some("http://127.0.0.1:1"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap();
        let app = App::new(paths, db, crate::crypto::Keyring::new(&[7; 32]), docker, selflog, 1).unwrap();
        let file = std::fs::File::create(log_path(&app, "r1")).ok();
        let log = RunLog { app: app.clone(), run_id: "r1".into(), project_id: None, service_id: None, verb: "Backed up", state: Mutex::new(LogState { file, ..Default::default() }) };
        let status = r#"{"message_type":"status","percent_done":0.35,"total_bytes":3435973837,"bytes_done":1288490189,"files_done":3,"total_files":9}"#;
        assert!(log.restic(status).is_none());
        assert!(log.restic(status).is_none());
        assert_eq!(progress("r1").unwrap()["files_done"], 3);
        assert!(log.restic(r#"{"message_type":"error","error":{"message":"permission denied"},"item":"/data/x"}"#).is_none());
        assert!(log.restic(r#"{"message_type":"exit_error","code":10,"message":"Fatal: repository does not exist\nIs there a repository?"}"#).is_none());
        assert!(log.restic("Fatal: unable to open repository").is_none());
        log.line("Failed: one\ntwo");
        let s = log.restic(r#"{"message_type":"summary","snapshot_id":"abc123","total_bytes_processed":42}"#).unwrap();
        assert_eq!(summary(&s), (Some("abc123".into()), Some(42)));
        let lines: Vec<String> = crate::deploy::read_log_file(&log_path(&app, "r1")).into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            lines,
            ["Backed up 1.2 GB of 3.2 GB (35%)", "/data/x: permission denied", "Fatal: repository does not exist", "Is there a repository?", "Fatal: unable to open repository", "Failed: one two"]
        );
        assert_eq!(Vec::from(util::lock(&log.state).tail.clone()), ["Fatal: repository does not exist", "Is there a repository?", "Fatal: unable to open repository"]);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", r#"echo '{"message_type":"summary","snapshot_id":"def"}'; echo '{"message_type":"status","percent_done":1}'; echo '{"message_type":"exit_error","message":"Fatal: no"}' >&2; exit 1"#]);
        assert_eq!(format!("{:#}", log.run(cmd).await.unwrap_err()), "restic failed: Fatal: no");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", r#"echo '{"message_type":"summary","snapshot_id":"def"}'"#]);
        assert_eq!(log.run(cmd).await.unwrap().unwrap()["snapshot_id"], "def");
        drop(log);
        assert!(progress("r1").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deep_archive_args() {
        let s3 = |class: &str| Target { id: "t".into(), kind: "s3".into(), config: serde_json::json!({ "storage_class": class }) };
        let local = Target { id: "t".into(), kind: "local".into(), config: serde_json::json!({ "storage_class": "DEEP_ARCHIVE" }) };
        assert_eq!(s3("STANDARD").storage_class(), None);
        assert_eq!(local.storage_class(), None);
        let forget = ["forget", "--tag", "job:x", "--keep-last", "7", "--prune"];
        let deep = restic_args(s3("DEEP_ARCHIVE").storage_class(), &forget);
        assert!(deep.windows(2).any(|w| w == ["-o", "s3.storage-class=DEEP_ARCHIVE"]), "{deep:?}");
        assert!(deep.ends_with(&["--prune".to_string(), "--repack-cacheable-only".to_string()]), "{deep:?}");
        let restore = restic_args(Some("DEEP_ARCHIVE"), &["restore", "abc"]);
        assert_eq!(restore[..2], ["-e", "RESTIC_FEATURES=s3-restore"]);
        assert!(restore.contains(&"s3.enable-restore=true".to_string()), "{restore:?}");
        assert_eq!(restic_args(Some("GLACIER_IR"), &forget)[..3], [RESTIC, "-o", "s3.storage-class=GLACIER_IR"]);
        for plain in [s3("STANDARD"), local] {
            let args = restic_args(plain.storage_class(), &forget);
            assert_eq!(args[0], RESTIC);
            assert!(!args.iter().any(|a| a.contains("storage-class") || a == "--repack-cacheable-only"), "{args:?}");
        }
    }

    /// A consumer that talks a lot before reading (restic's progress) must not deadlock the pipe.
    #[tokio::test]
    async fn pipe_drains_a_chatty_consumer() {
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        };
        let last = Mutex::new(String::new());
        let keep = |l: &str, _| *util::lock(&last) = l.to_string();
        let piped = pipe(sh("head -c 300000 /dev/zero"), sh("head -c 300000 /dev/zero | tr '\\0' x; wc -c"), &keep);
        tokio::time::timeout(Duration::from_secs(20), piped).await.expect("the pipe stalled").unwrap();
        let out = last.into_inner().unwrap();
        assert!(out.trim_end().ends_with("300000"), "{}", out.len());
    }
}
