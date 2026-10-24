//! Backups with restic (run from its official image). Databases are dumped through the
//! container's init and streamed in, volumes are read directly, and Tugport's own state
//! is snapshotted. Repositories are encrypted with a password derived from the master key.

use crate::app::{App, AppState};
use crate::model::Engine;
use crate::{databases, runtime, store, util};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    c.args(["run", "--rm", "-i", "--hostname", "tugport", "--env-file"]).arg(&repo.env_file);
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
async fn thaw(cli: Command, repo: &Repo, snapshot: &str, include: &str) -> Result<()> {
    if repo.archived() {
        crate::proc::output(restic_on(cli, repo, &[], &["restore", snapshot, "--target", "/thaw", "--include", include])).await.context("thawing the backup from Deep Archive")?;
    }
    Ok(())
}

/// Whether a job's backups are in Deep Archive, where a restore first waits up to 48 hours for S3.
pub fn archived(app: &App, j: &Job) -> bool {
    target(app, &j.target_id).is_ok_and(|t| t.storage_class() == Some(DEEP_ARCHIVE))
}

async fn ensure_repo(repo: &Repo) -> Result<()> {
    let mut check = restic_cmd(repo, &[], &["cat", "config"]);
    check.stdout(Stdio::null()).stderr(Stdio::null());
    if check.status().await?.success() {
        return Ok(());
    }
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

/// Run `producer`'s stdout into `consumer`'s stdin; returns the consumer's stdout.
pub async fn pipe(mut producer: Command, mut consumer: Command) -> Result<String> {
    producer.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
    consumer.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut p = producer.spawn()?;
    let mut c = consumer.spawn()?;
    let mut from = p.stdout.take().ok_or_else(|| anyhow!("no producer stdout"))?;
    let mut to = c.stdin.take().ok_or_else(|| anyhow!("no consumer stdin"))?;
    let copy = async move {
        let n = tokio::io::copy(&mut from, &mut to).await;
        drop(to);
        n
    };
    // All at once: a consumer that writes a lot (restic's progress) must not stall the copy.
    let (p_out, c_out, copied) = tokio::join!(p.wait_with_output(), c.wait_with_output(), copy);
    let (p_out, c_out) = (p_out?, c_out?);
    if !p_out.status.success() {
        bail!("the dump failed: {}", String::from_utf8_lossy(&p_out.stderr).trim());
    }
    if !c_out.status.success() {
        bail!("restic failed: {}", String::from_utf8_lossy(&c_out.stderr).trim());
    }
    if copied? == 0 {
        bail!("the dump was empty");
    }
    Ok(String::from_utf8_lossy(&c_out.stdout).to_string())
}

fn summary(json_lines: &str) -> (Option<String>, Option<i64>) {
    for line in json_lines.lines().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v["message_type"] == "summary"
        {
            let size = v["total_bytes_processed"].as_i64().or(v["data_added"].as_i64());
            return (v["snapshot_id"].as_str().map(String::from), size);
        }
    }
    (None, None)
}

/// Runs left mid-way by a crash or restart: mark them failed.
pub fn recover_interrupted(app: &App) -> Result<()> {
    app.db.exec("UPDATE backup_runs SET status = 'failed', error = 'Tugport restarted during this run', finished_at = ?1 WHERE status = 'running'", [util::now()])?;
    Ok(())
}

/// Record a run (backup, restore or test-restore) as started; returns its id.
fn start_run(app: &App, job_id: &str, kind: &str, snapshot: Option<&str>) -> Result<String> {
    let id = util::id();
    app.db.exec(
        "INSERT INTO backup_runs (id, job_id, kind, status, snapshot, started_at) VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
        params![id, job_id, kind, snapshot, util::now()],
    )?;
    Ok(id)
}

/// Record how a run ended; a restore keeps the snapshot it started from.
fn end_run(app: &App, id: &str, error: Option<&str>, snapshot: Option<&str>, size: Option<i64>) -> Result<()> {
    let status = if error.is_some() { "failed" } else { "ok" };
    app.db.exec(
        "UPDATE backup_runs SET status = ?1, error = ?2, snapshot = COALESCE(?3, snapshot), size_bytes = ?4, finished_at = ?5 WHERE id = ?6",
        params![status, error, snapshot, size, util::now(), id],
    )?;
    Ok(())
}

/// Run a backup job now. Returns the run id.
pub async fn run_job(app: &AppState, job_id: &str) -> Result<String> {
    let j = job(app, job_id)?;
    let run_id = start_run(app, &j.id, "backup", None)?;
    let result = backup(app, &j).await;
    match &result {
        Ok((snapshot, size)) => end_run(app, &run_id, None, snapshot.as_deref(), *size)?,
        Err(e) => {
            let msg = format!("{e:#}");
            end_run(app, &run_id, Some(&msg), None, None)?;
            let s = j.service_id.as_deref().and_then(|id| store::service(app, id).ok().flatten());
            let what = s.as_ref().map(|s| format!("{}/{}", s.project, s.name)).unwrap_or_else(|| "Tugport".into());
            crate::notify::event(app, "backup.failed", &format!("Backup of {what} failed"), &msg, s.as_ref()).await;
        }
    }
    app.emit("backup", None, j.service_id.as_deref(), serde_json::json!({ "run_id": run_id, "ok": result.is_ok() }));
    result.map(|_| run_id)
}

/// A database job's service and engine.
fn database(app: &App, j: &Job) -> Result<(store::Service, Engine)> {
    let sid = j.service_id.as_deref().ok_or_else(|| anyhow!("database backups need a service"))?;
    let s = store::service(app, sid)?.ok_or_else(|| anyhow!("the service was deleted"))?;
    let engine = s.spec.database.as_ref().map(|d| d.engine).ok_or_else(|| anyhow!("{} is not a database", s.name))?;
    Ok((s, engine))
}

async fn backup(app: &App, j: &Job) -> Result<(Option<String>, Option<i64>)> {
    let t = target(app, &j.target_id)?;
    let repo = repo(app, &t)?;
    ensure_repo(&repo).await?;
    let job_tag = format!("job:{}", j.id);
    let out = match j.kind.as_str() {
        "database" => {
            let (s, engine) = database(app, j)?;
            let ctr = runtime::running_replica(app, &s).await?;
            let dump = exec_cli(app, &ctr.node, &ctr.id, false, databases::preset(engine).dump)?;
            let filename = format!("{}/{}", s.name, databases::preset(engine).dump_file);
            let restic = restic_cmd(&repo, &[], &["backup", "--json", "--stdin", "--stdin-filename", &filename, "--tag", "tugport", "--tag", &job_tag]);
            pipe(dump, restic).await?
        }
        "volume" => {
            let vol = volume_name(j)?;
            let mount = format!("{vol}:/data/{vol}:ro");
            let cli = crate::nodes::docker_cli(app, &volume_node(app, j))?;
            crate::proc::output(restic_on(cli, &repo, &[mount], &["backup", "--json", &format!("/data/{vol}"), "--tag", "tugport", "--tag", &job_tag])).await?
        }
        "state" => {
            let dir = app.paths.data.join("backup-staging");
            std::fs::create_dir_all(&dir)?;
            app.db.snapshot(&dir.join("tugport.db"))?;
            let mount = format!("{}:/data/tugport:ro", dir.display());
            let out = crate::proc::output(restic_cmd(&repo, &[mount], &["backup", "--json", "/data/tugport", "--tag", "tugport", "--tag", &job_tag])).await;
            let _ = std::fs::remove_file(dir.join("tugport.db"));
            out?
        }
        other => bail!("unknown backup kind {other}"),
    };
    let (snapshot, size) = summary(&out);
    let keep = j.keep_last.max(1).to_string();
    crate::proc::output(restic_cmd(&repo, &[], &["forget", "--tag", &job_tag, "--keep-last", &keep, "--prune"])).await.context("applying retention")?;
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
    let restore_id = start_run(app, &j.id, if test { "test-restore" } else { "restore" }, Some(&snapshot))?;
    let result = match j.kind.as_str() {
        // Reading archived data takes S3 up to 48 hours, so a test checks only what is in Standard.
        _ if test && repo.archived() => verify_snapshot(&repo, &snapshot).await,
        "database" => restore_database(app, &j, &repo, &snapshot, test).await,
        "volume" if !test => restore_volume(app, &j, &repo, &snapshot).await,
        "volume" => verify_snapshot(&repo, &snapshot).await,
        "state" => restore_state(app, &repo, &snapshot).await,
        other => Err(anyhow!("unknown backup kind {other}")),
    };
    end_run(app, &restore_id, result.as_ref().err().map(|e| format!("{e:#}")).as_deref(), None, None)?;
    result
}

async fn verify_snapshot(repo: &Repo, snapshot: &str) -> Result<String> {
    // Without --read-data, check reads only the index and folder trees, which stay in Standard.
    let check: &[&str] = if repo.archived() { &["check"] } else { &["check", "--read-data-subset=5%"] };
    crate::proc::output(restic_cmd(repo, &[], check)).await?;
    crate::proc::output(restic_cmd(repo, &[], &["ls", snapshot])).await?;
    if repo.archived() {
        return Ok("The snapshot is listed and the repository's index and folder trees passed their integrity check. The data itself is in S3 Glacier Deep Archive, so it was not read; a real restore thaws it first, which takes up to 48 hours.".into());
    }
    Ok("The snapshot is readable and the repository passed its integrity check.".into())
}

async fn restore_database(app: &AppState, j: &Job, repo: &Repo, snapshot: &str, test: bool) -> Result<String> {
    let (s, engine) = database(app, j)?;
    if engine == Engine::Redis && !test {
        bail!("restoring Redis replaces its data file; stop the service and use the volume backup instead");
    }
    let db = databases::preset(engine);
    let path = format!("/{}/{}", s.name, db.dump_file);
    // `restic dump` cannot thaw archived data itself.
    thaw(Command::new("docker"), repo, snapshot, &path).await?;
    let (node, container, scratch_name) = if test {
        let (node, id, name) = scratch_database(app, &s).await?;
        (node, id, Some(name))
    } else {
        let r = runtime::running_replica(app, &s).await?;
        (r.node, r.id, None)
    };
    // The temporary copy goes whatever happens from here on.
    let result = async {
        let load = exec_cli(app, &node, &container, true, db.restore)?;
        pipe(restic_cmd(repo, &[], &["dump", snapshot, &path]), load).await?;
        if !test {
            return Ok("Restored in place.".to_string());
        }
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

async fn restore_volume(app: &AppState, j: &Job, repo: &Repo, snapshot: &str) -> Result<String> {
    let vol = volume_name(j)?;
    // Before anything stops, so the service keeps running while S3 thaws the data.
    thaw(crate::nodes::docker_cli(app, &volume_node(app, j))?, repo, snapshot, &format!("/data/{vol}")).await?;
    let users: Vec<runtime::Replica> = match &j.service_id {
        Some(sid) => runtime::replicas(app, sid).await?.into_iter().filter(|r| r.state == "running").collect(),
        None => vec![],
    };
    // Whatever was stopped starts again, also when stopping another one or the restore fails.
    let mut stopped = vec![];
    let res = async {
        for r in &users {
            runtime::stop(app, &r.node, &r.id, 30).await?;
            stopped.push(r);
        }
        let mount = format!("{vol}:/data/{vol}");
        let cli = crate::nodes::docker_cli(app, &volume_node(app, j))?;
        crate::proc::output(restic_on(cli, repo, &[mount], &["restore", snapshot, "--target", "/", "--include", &format!("/data/{vol}")])).await
    }
    .await;
    for r in stopped {
        if let Err(e) = runtime::start(app, &r.node, &r.id).await {
            tracing::warn!("starting {} again after restoring {vol}: {e:#}", r.name);
        }
    }
    // Restarted containers may have new addresses, also when the restore failed.
    crate::proxy::reload(app);
    res?;
    Ok(format!("Restored volume {vol}."))
}

async fn restore_state(app: &App, repo: &Repo, snapshot: &str) -> Result<String> {
    let dir = app.paths.data.join("restored");
    std::fs::create_dir_all(&dir)?;
    let mount = format!("{}:/out", dir.display());
    crate::proc::output(restic_cmd(repo, &[mount], &["restore", snapshot, "--target", "/out"])).await?;
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
        let out = "{\"message_type\":\"status\"}\n{\"message_type\":\"summary\",\"snapshot_id\":\"abc123\",\"total_bytes_processed\":42}\n";
        assert_eq!(summary(out), (Some("abc123".into()), Some(42)));
        assert!(valid_schedule("0 3 * * *"));
        assert!(!valid_schedule("every day"));
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
        let piped = pipe(sh("head -c 300000 /dev/zero"), sh("head -c 300000 /dev/zero | tr '\\0' x; wc -c"));
        let out = tokio::time::timeout(Duration::from_secs(20), piped).await.expect("the pipe stalled").unwrap();
        assert!(out.trim_end().ends_with("300000"), "{}", out.len());
    }
}
