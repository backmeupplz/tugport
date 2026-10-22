//! Backups with restic (run from its official image). Databases are dumped through the
//! container's init and streamed in, volumes are read directly, and Tugport's own state
//! is snapshotted. Repositories are encrypted with a password derived from the master key.

use crate::app::{App, AppState};
use crate::model::Engine;
use crate::{databases, runtime, store, util};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const RESTIC: &str = "restic/restic:0.19.1";

pub struct Target {
    pub id: String,
    pub kind: String,
    pub config: Value,
}

pub struct Job {
    pub id: String,
    pub service_id: Option<String>,
    pub target_id: String,
    pub kind: String,
    pub volume: Option<String>,
    pub schedule: String,
    pub keep_last: i64,
}

pub fn target(app: &App, id: &str) -> Result<Target> {
    let (kind, config): (String, Vec<u8>) = app
        .db
        .conn()
        .query_row("SELECT kind, config FROM backup_targets WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| anyhow!("backup target {id} not found"))?;
    let config = serde_json::from_str(&app.keys.open_str(&format!("backup-target:{id}"), &config)?)?;
    Ok(Target { id: id.into(), kind, config })
}

pub fn job(app: &App, id: &str) -> Result<Job> {
    app.db
        .conn()
        .query_row("SELECT id, service_id, target_id, kind, volume, schedule, keep_last FROM backup_jobs WHERE id = ?1", [id], |r| {
            Ok(Job { id: r.get(0)?, service_id: r.get(1)?, target_id: r.get(2)?, kind: r.get(3)?, volume: r.get(4)?, schedule: r.get(5)?, keep_last: r.get(6)? })
        })
        .optional()?
        .ok_or_else(|| anyhow!("backup job {id} not found"))
}

/// Restic settings for a target, written to a memory-only env file for `docker run --env-file`.
struct Repo {
    env_file: PathBuf,
    mounts: Vec<String>,
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
    let env_file = app.paths.run.join(format!("restic-{}.env", util::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&env_file)?;
    for (k, v) in env {
        writeln!(f, "{k}={v}")?;
    }
    Ok(Repo { env_file, mounts })
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
    c.arg(RESTIC).args(args).kill_on_drop(true);
    c
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

fn dump_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "dump.pgc",
        Engine::Mysql | Engine::Mariadb => "dump.sql",
        Engine::Mongo => "dump.archive.gz",
        Engine::Redis => "dump.rdb",
    }
}

/// The server a job's volume lives on: its service's server, else this one.
fn volume_node(app: &App, j: &Job) -> String {
    j.service_id.as_ref().and_then(|sid| app.db.get_state(&format!("pin:{sid}")).ok().flatten()).unwrap_or_else(|| "local".into())
}

/// The running container of a service's live deploy.
async fn live_container(app: &App, service_id: &str) -> Result<runtime::Replica> {
    let live = runtime::live_deploy(app, service_id);
    runtime::replicas(app, service_id)
        .await?
        .into_iter()
        .find(|r| r.state == "running" && live.as_deref().is_none_or(|l| r.deploy == l))
        .ok_or_else(|| anyhow!("the service is not running"))
}

/// Run `producer`'s stdout into `consumer`'s stdin; returns the consumer's stdout.
async fn pipe(mut producer: Command, mut consumer: Command) -> Result<String> {
    producer.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
    consumer.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut p = producer.spawn()?;
    let mut c = consumer.spawn()?;
    let mut from = p.stdout.take().ok_or_else(|| anyhow!("no producer stdout"))?;
    let mut to = c.stdin.take().ok_or_else(|| anyhow!("no consumer stdin"))?;
    let copy = tokio::spawn(async move {
        let n = tokio::io::copy(&mut from, &mut to).await;
        drop(to);
        n
    });
    let p_out = p.wait_with_output().await?;
    let copied = copy.await.map_err(|e| anyhow!("copy task failed: {e}"))??;
    let c_out = c.wait_with_output().await?;
    if !p_out.status.success() {
        bail!("the dump failed: {}", String::from_utf8_lossy(&p_out.stderr).trim());
    }
    if !c_out.status.success() {
        bail!("restic failed: {}", String::from_utf8_lossy(&c_out.stderr).trim());
    }
    if copied == 0 {
        bail!("the dump was empty");
    }
    Ok(String::from_utf8_lossy(&c_out.stdout).to_string())
}

fn summary(json_lines: &str) -> (Option<String>, Option<i64>) {
    for line in json_lines.lines().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && v["message_type"] == "summary" {
                let size = v["total_bytes_processed"].as_i64().or(v["data_added"].as_i64());
                return (v["snapshot_id"].as_str().map(String::from), size);
            }
    }
    (None, None)
}

/// Run a backup job now. Returns the run id.
pub async fn run_job(app: &AppState, job_id: &str) -> Result<String> {
    let j = job(app, job_id)?;
    let run_id = util::id();
    app.db.conn().execute(
        "INSERT INTO backup_runs (id, job_id, kind, status, started_at) VALUES (?1, ?2, 'backup', 'running', ?3)",
        params![run_id, j.id, util::now()],
    )?;
    let result = backup(app, &j).await;
    let now = util::now();
    match &result {
        Ok((snapshot, size)) => {
            app.db.conn().execute(
                "UPDATE backup_runs SET status = 'ok', snapshot = ?1, size_bytes = ?2, finished_at = ?3 WHERE id = ?4",
                params![snapshot, size, now, run_id],
            )?;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            app.db.conn().execute("UPDATE backup_runs SET status = 'failed', error = ?1, finished_at = ?2 WHERE id = ?3", params![msg, now, run_id])?;
            let s = j.service_id.as_deref().and_then(|id| store::service(app, id).ok().flatten());
            let what = s.as_ref().map(|s| format!("{}/{}", s.project, s.name)).unwrap_or_else(|| "Tugport".into());
            crate::notify::event(app, "backup.failed", &format!("Backup of {what} failed"), &msg, s.as_ref()).await;
        }
    }
    app.emit("backup", None, j.service_id.as_deref(), serde_json::json!({ "run_id": run_id, "ok": result.is_ok() }));
    result.map(|_| run_id)
}

async fn backup(app: &App, j: &Job) -> Result<(Option<String>, Option<i64>)> {
    let t = target(app, &j.target_id)?;
    let repo = repo(app, &t)?;
    ensure_repo(&repo).await?;
    let job_tag = format!("job:{}", j.id);
    let out = match j.kind.as_str() {
        "database" => {
            let sid = j.service_id.as_deref().ok_or_else(|| anyhow!("database backups need a service"))?;
            let s = store::service(app, sid)?.ok_or_else(|| anyhow!("the service was deleted"))?;
            let engine = s.spec.database.as_ref().map(|d| d.engine).ok_or_else(|| anyhow!("{} is not a database", s.name))?;
            let ctr = live_container(app, &s.id).await?;
            let mut dump = crate::nodes::docker_cli(app, &ctr.node)?;
            dump.args(["exec", &ctr.id, crate::secrets::INIT_PATH, "exec", "--", "sh", "-c", databases::dump_command(engine)]);
            let filename = format!("{}/{}", s.name, dump_name(engine));
            let restic = restic_cmd(&repo, &[], &["backup", "--json", "--stdin", "--stdin-filename", &filename, "--tag", "tugport", "--tag", &job_tag]);
            pipe(dump, restic).await?
        }
        "volume" => {
            let vol = j.volume.as_deref().ok_or_else(|| anyhow!("volume backups need a volume"))?;
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
        app.db.conn().query_row("SELECT job_id, snapshot FROM backup_runs WHERE id = ?1", [run_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let snapshot = snapshot.ok_or_else(|| anyhow!("this run has no snapshot"))?;
    let j = job(app, &job_id)?;
    let t = target(app, &j.target_id)?;
    let repo = repo(app, &t)?;
    let restore_id = util::id();
    app.db.conn().execute(
        "INSERT INTO backup_runs (id, job_id, kind, status, snapshot, started_at) VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
        params![restore_id, j.id, if test { "test-restore" } else { "restore" }, snapshot, util::now()],
    )?;
    let result = match j.kind.as_str() {
        "database" => restore_database(app, &j, &repo, &snapshot, test).await,
        "volume" if !test => restore_volume(app, &j, &repo, &snapshot).await,
        "volume" => verify_snapshot(&repo, &snapshot).await,
        "state" => restore_state(app, &repo, &snapshot).await,
        other => Err(anyhow!("unknown backup kind {other}")),
    };
    let (status, err) = match &result {
        Ok(_) => ("ok", None),
        Err(e) => ("failed", Some(format!("{e:#}"))),
    };
    app.db.conn().execute("UPDATE backup_runs SET status = ?1, error = ?2, finished_at = ?3 WHERE id = ?4", params![status, err, util::now(), restore_id])?;
    result
}

async fn verify_snapshot(repo: &Repo, snapshot: &str) -> Result<String> {
    crate::proc::output(restic_cmd(repo, &[], &["check", "--read-data-subset=5%"])).await?;
    crate::proc::output(restic_cmd(repo, &[], &["ls", snapshot])).await?;
    Ok("The snapshot is readable and the repository passed its integrity check.".into())
}

async fn restore_database(app: &AppState, j: &Job, repo: &Repo, snapshot: &str, test: bool) -> Result<String> {
    let sid = j.service_id.as_deref().ok_or_else(|| anyhow!("no service"))?;
    let s = store::service(app, sid)?.ok_or_else(|| anyhow!("the service was deleted"))?;
    let engine = s.spec.database.as_ref().map(|d| d.engine).ok_or_else(|| anyhow!("not a database"))?;
    if engine == Engine::Redis && !test {
        bail!("restoring Redis replaces its data file; stop the service and use the volume backup instead");
    }
    let path = format!("/{}/{}", s.name, dump_name(engine));
    let (node, container, scratch_name) = if test {
        let (node, id, name) = scratch_database(app, &s).await?;
        (node, id, Some(name))
    } else {
        let r = live_container(app, &s.id).await?;
        (r.node, r.id, None)
    };
    let dump = restic_cmd(repo, &[], &["dump", snapshot, &path]);
    let mut load = crate::nodes::docker_cli(app, &node)?;
    load.args(["exec", "-i", &container, crate::secrets::INIT_PATH, "exec", "--", "sh", "-c", databases::restore_command(engine)]);
    let result = pipe(dump, load).await;
    let check = if test && result.is_ok() { check_database(app, engine, &node, &container).await } else { Ok(String::new()) };
    if let Some(name) = scratch_name {
        let _ = runtime::remove(app, &node, &container, &name).await;
    }
    result?;
    let detail = check?;
    Ok(if test { format!("Test restore succeeded. {detail}") } else { "Restored in place.".into() })
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
    if let Err(e) = runtime::start(app, &node.id, &id).await {
        let _ = runtime::remove(app, &node.id, &id, &name).await;
        return Err(e);
    }
    if let Err(e) = runtime::wait_healthy(app, &copy, &node.id, &id, &network, Duration::from_secs(120), img.has_healthcheck, &|_| {}).await {
        let _ = runtime::remove(app, &node.id, &id, &name).await;
        return Err(e.context("the temporary database did not start"));
    }
    Ok((node.id.clone(), id, name))
}

async fn check_database(app: &App, engine: Engine, node: &str, container: &str) -> Result<String> {
    let query = match engine {
        Engine::Postgres => "psql -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -tAc \"select count(*) from information_schema.tables where table_schema not in ('pg_catalog','information_schema')\"",
        Engine::Mysql => "mysql -uroot -p\"$MYSQL_ROOT_PASSWORD\" -N -e \"select count(*) from information_schema.tables where table_schema not in ('mysql','sys','information_schema','performance_schema')\"",
        Engine::Mariadb => "mariadb -uroot -p\"$MARIADB_ROOT_PASSWORD\" -N -e \"select count(*) from information_schema.tables where table_schema not in ('mysql','sys','information_schema','performance_schema')\"",
        Engine::Mongo => "mongosh --quiet -u \"$MONGO_INITDB_ROOT_USERNAME\" -p \"$MONGO_INITDB_ROOT_PASSWORD\" --authenticationDatabase admin --eval 'db.adminCommand({listDatabases:1}).databases.length'",
        Engine::Redis => "redis-cli -a \"$REDIS_PASSWORD\" --no-auth-warning dbsize",
    };
    let mut c = crate::nodes::docker_cli(app, node)?;
    c.args(["exec", container, crate::secrets::INIT_PATH, "exec", "--", "sh", "-c", query]);
    let out = crate::proc::output(c).await?;
    Ok(format!("The restored copy reports {} tables/collections/keys.", out.trim()))
}

async fn restore_volume(app: &AppState, j: &Job, repo: &Repo, snapshot: &str) -> Result<String> {
    let vol = j.volume.as_deref().ok_or_else(|| anyhow!("no volume"))?;
    let users: Vec<runtime::Replica> = match &j.service_id {
        Some(sid) => runtime::replicas(app, sid).await?.into_iter().filter(|r| r.state == "running").collect(),
        None => vec![],
    };
    for r in &users {
        runtime::stop(app, &r.node, &r.id, 30).await?;
    }
    let mount = format!("{vol}:/data/{vol}");
    let cli = crate::nodes::docker_cli(app, &volume_node(app, j))?;
    let res = crate::proc::output(restic_on(cli, repo, &[mount], &["restore", snapshot, "--target", "/", "--include", &format!("/data/{vol}")])).await;
    for r in &users {
        let _ = runtime::start(app, &r.node, &r.id).await;
    }
    res?;
    crate::proxy::reload(app);
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
    use std::str::FromStr;
    let running: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>> = Default::default();
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let jobs: Vec<(String, String)> = {
            let conn = app.db.conn();
            let Ok(mut stmt) = conn.prepare("SELECT id, schedule FROM backup_jobs WHERE enabled = 1") else { continue };
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|m| m.filter_map(|r| r.ok()).collect()).unwrap_or_default()
        };
        let now = chrono::Utc::now();
        for (id, schedule) in jobs {
            let Ok(cron) = croner::Cron::from_str(&schedule) else { continue };
            let last: i64 = app
                .db
                .conn()
                .query_row("SELECT COALESCE(MAX(started_at), 0) FROM backup_runs WHERE job_id = ?1 AND kind = 'backup'", [&id], |r| r.get(0))
                .unwrap_or(0);
            let created: i64 = app.db.conn().query_row("SELECT created_at FROM backup_jobs WHERE id = ?1", [&id], |r| r.get(0)).unwrap_or(0);
            let from = chrono::DateTime::from_timestamp(last.max(created), 0).unwrap_or(now);
            let Ok(next) = cron.find_next_occurrence(&from, false) else { continue };
            if next > now || !crate::util::lock(&running).insert(id.clone()) {
                continue;
            }
            let (app, running) = (app.clone(), running.clone());
            tokio::spawn(async move {
                if let Err(e) = run_job(&app, &id).await {
                    tracing::warn!("backup job {id} failed: {e:#}");
                }
                crate::util::lock(&running).remove(&id);
            });
        }
    }
}

pub fn valid_schedule(s: &str) -> bool {
    use std::str::FromStr;
    croner::Cron::from_str(s).is_ok()
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
}
