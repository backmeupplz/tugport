//! Backup targets (S3 or local), jobs, runs and restores.

use super::service_or_404;
use super::{ApiError, ApiResult, Auth, Events, Owner, ok, sse_events, sse_json};
use super::{bad, detached, row_json};
use crate::app::AppState;
use crate::auth::Role;
use crate::{backup, util};
use axum::Json;
use axum::extract::{Path, State};
use axum::response::sse;
use futures_util::future::ready;
use futures_util::stream::{self, BoxStream, StreamExt};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

pub async fn list_targets(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_servers("needs the owner or a role on all projects")?;
    let mut rows = app.db.rows("SELECT id, name, kind, created_at FROM backup_targets ORDER BY created_at", [], row_json)?;
    for row in &mut rows {
        if let Some(id) = row["id"].as_str()
            && let Ok(t) = backup::target(&app, id)
        {
            row["storage_class"] = json!(t.storage_class());
        }
    }
    Ok(Json(json!({ "targets": rows })))
}

#[derive(Deserialize)]
pub struct TargetBody {
    name: String,
    kind: String,
    config: Value,
}

pub async fn create_target(State(app): State<AppState>, Owner(a): Owner, Json(b): Json<TargetBody>) -> ApiResult<Json<Value>> {
    let need: &[&str] = match b.kind.as_str() {
        "s3" => &["bucket", "access_key", "secret_key"],
        "local" => &["path"],
        _ => return Err(ApiError::bad_request("kind is s3 or local")),
    };
    for k in need {
        if b.config.get(*k).and_then(|v| v.as_str()).is_none_or(|v| v.is_empty()) {
            return Err(ApiError::bad_request(format!("{} targets need {k}", b.kind)));
        }
    }
    if let Some(class) = b.config.get("storage_class")
        && !class.is_null()
        && !(b.kind == "s3" && class.as_str().is_some_and(|c| backup::STORAGE_CLASSES.contains(&c)))
    {
        return Err(ApiError::bad_request("storage_class is STANDARD, STANDARD_IA, GLACIER_IR or DEEP_ARCHIVE, and only s3 targets have one"));
    }
    if b.kind == "local" && !b.config["path"].as_str().unwrap_or("").starts_with('/') {
        return Err(ApiError::bad_request("local targets need an absolute path"));
    }
    let id = util::id();
    let sealed = app.keys.seal_str(&format!("backup-target:{id}"), &b.config.to_string())?;
    app.db.exec(
        "INSERT INTO backup_targets (id, name, kind, config, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, b.name, b.kind, sealed, util::now()],
    )?;
    a.audit(&app, "backup.target_create", Some(&b.name), Some(&b.kind));
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_target(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.db.exec("DELETE FROM backup_targets WHERE id = ?1", [&id])?;
    a.audit(&app, "backup.target_delete", Some(&id), None);
    Ok(ok())
}

/// The project of a job's service (None for jobs not tied to a service, which are the owner's).
fn project_of(app: &AppState, service_id: Option<&str>) -> ApiResult<Option<String>> {
    Ok(match service_id {
        Some(sid) => crate::store::service(app, sid)?.map(|s| s.project_id),
        None => None,
    })
}

fn job_project(app: &AppState, job_id: &str) -> ApiResult<(backup::Job, Option<String>)> {
    let j = backup::job(app, job_id).map_err(|_| ApiError::not_found("backup job not found"))?;
    let project = project_of(app, j.service_id.as_deref())?;
    Ok((j, project))
}

fn require_job(a: &Auth, project: &Option<String>, role: Role) -> ApiResult<()> {
    match project {
        Some(p) => a.require(p, role),
        None => a.require_owner(),
    }
}

pub async fn list_jobs(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let jobs = app.db.rows("SELECT id, service_id, target_id, kind, volume, schedule, keep_last, enabled FROM backup_jobs ORDER BY created_at", [], row_json)?;
    let mut out = vec![];
    for mut job in jobs {
        if require_job(&a, &project_of(&app, job["service_id"].as_str())?, Role::Viewer).is_err() {
            continue;
        }
        job["enabled"] = json!(job["enabled"] != 0);
        let mut last = app.db.row(
            "SELECT id, status, finished_at, error FROM backup_runs WHERE job_id = ?1 AND kind = 'backup' ORDER BY started_at DESC LIMIT 1",
            [job["id"].as_str()],
            row_json,
        )?;
        if let Some(run) = &mut last {
            with_progress(run);
        }
        job["last_run"] = json!(last);
        out.push(job);
    }
    Ok(Json(json!({ "jobs": out })))
}

#[derive(Deserialize)]
pub struct JobBody {
    service_id: Option<String>,
    target_id: String,
    kind: String,
    volume: Option<String>,
    schedule: String,
    #[serde(default = "seven")]
    keep_last: i64,
}

fn seven() -> i64 {
    7
}

pub async fn create_job(State(app): State<AppState>, a: Auth, Json(b): Json<JobBody>) -> ApiResult<Json<Value>> {
    if !backup::valid_schedule(&b.schedule) {
        return Err(ApiError::bad_request("the schedule is not a valid cron expression, for example \"0 3 * * *\""));
    }
    let service = match &b.service_id {
        Some(sid) => Some(service_or_404(&app, &a, sid, Role::Admin)?),
        None => {
            a.require_owner()?;
            None
        }
    };
    check_job(&b.kind, b.volume.as_deref(), service.as_ref().map(|s| &s.spec)).map_err(ApiError::bad_request)?;
    backup::target(&app, &b.target_id).map_err(|_| ApiError::bad_request("unknown backup target"))?;
    let id = util::id();
    app.db.exec(
        "INSERT INTO backup_jobs (id, service_id, target_id, kind, volume, schedule, keep_last, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, service.as_ref().map(|s| s.id.clone()), b.target_id, b.kind, b.volume, b.schedule, b.keep_last.max(1), util::now()],
    )?;
    app.audit(Some(&a.0), "backup.job_create", service.as_ref().map(|s| s.project_id.as_str()), service.as_ref().map(|s| s.id.as_str()), Some(&b.kind), Some(&b.schedule));
    Ok(Json(json!({ "id": id })))
}

/// What a job of `kind` may back up, for the service (spec) it is tied to.
fn check_job(kind: &str, volume: Option<&str>, spec: Option<&crate::model::ServiceSpec>) -> Result<(), &'static str> {
    let volume = volume.unwrap_or("");
    match (kind, spec) {
        ("database", Some(s)) if s.database.is_some() => Ok(()),
        ("database", _) => Err("database backups need a database service"),
        ("volume", _) if volume.is_empty() => Err("volume backups need a volume name"),
        // The name reaches `docker run -v`, where a path is a folder of the server: only one the
        // service already mounts. Another service's volume or folder would be another project's data.
        ("volume", Some(s)) if volume.starts_with('/') && !backup::bind_source(s, volume) => Err("pick one of the folders the service mounts"),
        ("volume", Some(s))
            if !volume.starts_with('/') && !s.mounts.iter().any(|m| matches!(m, crate::model::Mount::Volume { name, .. } if name == volume)) =>
        {
            Err("pick one of the service's volumes")
        }
        ("volume", None) if volume.contains(['/', ':']) => Err("that is not a volume name"),
        ("volume", _) | ("state", None) => Ok(()),
        ("state", Some(_)) => Err("state backups are not tied to a service"),
        _ => Err("kind is database, volume or state"),
    }
}

#[derive(Deserialize)]
pub struct JobPatch {
    schedule: Option<String>,
    keep_last: Option<i64>,
    enabled: Option<bool>,
}

pub async fn update_job(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<JobPatch>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Admin)?;
    if let Some(s) = &b.schedule
        && !backup::valid_schedule(s) {
            return Err(ApiError::bad_request("the schedule is not a valid cron expression"));
        }
    app.db.exec(
        "UPDATE backup_jobs SET schedule = COALESCE(?1, schedule), keep_last = COALESCE(?2, keep_last), enabled = COALESCE(?3, enabled) WHERE id = ?4",
        params![b.schedule, b.keep_last.map(|k| k.max(1)), b.enabled, j.id],
    )?;
    app.audit(Some(&a.0), "backup.job_update", project.as_deref(), j.service_id.as_deref(), Some(&j.kind), None);
    Ok(ok())
}

pub async fn delete_job(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Admin)?;
    app.db.exec("DELETE FROM backup_jobs WHERE id = ?1", [&j.id])?;
    app.audit(Some(&a.0), "backup.job_delete", project.as_deref(), j.service_id.as_deref(), Some(&j.kind), None);
    Ok(ok())
}

pub async fn run_job(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Operator)?;
    app.audit(Some(&a.0), "backup.run", project.as_deref(), j.service_id.as_deref(), Some(&j.kind), None);
    // The run finishes even if the caller goes away.
    let (app2, jid) = (app.clone(), j.id.clone());
    let run = detached(async move { backup::run_job(&app2, &jid).await }).await?.map_err(bad)?;
    Ok(Json(json!({ "run_id": run })))
}

pub async fn list_runs(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Viewer)?;
    let mut runs = app.db.rows(
        "SELECT id, kind, status, snapshot, size_bytes, error, started_at, finished_at FROM backup_runs WHERE job_id = ?1 ORDER BY started_at DESC LIMIT 100",
        [&j.id],
        row_json,
    )?;
    runs.iter_mut().for_each(with_progress);
    Ok(Json(json!({ "runs": runs })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RestoreBody {
    test: bool,
    /// With `test` on a Deep Archive backup: thaw the data and restore it too, not only check the index.
    full: bool,
}

pub async fn restore(State(app): State<AppState>, a: Auth, Path(run_id): Path<String>, body: Option<Json<RestoreBody>>) -> ApiResult<Json<Value>> {
    let RestoreBody { test, full } = body.map(|b| b.0).unwrap_or_default();
    let job_id: String = app.db.row("SELECT job_id FROM backup_runs WHERE id = ?1", [&run_id], |r| r.get(0))?.ok_or_else(|| ApiError::not_found("backup run not found"))?;
    let (j, project) = job_project(&app, &job_id)?;
    if test {
        require_job(&a, &project, Role::Operator)?;
    } else {
        // Restoring overwrites live data.
        match &project {
            Some(p) if a.0.can_destroy(p) => {}
            None if a.0.is_owner => {}
            _ => return Err(ApiError::forbidden("restoring in place needs the admin role and the destroy grant")),
        }
    }
    app.audit(Some(&a.0), if test { "backup.test_restore" } else { "backup.restore" }, project.as_deref(), j.service_id.as_deref(), Some(&run_id), None);
    let (app2, rid) = (app.clone(), run_id.clone());
    if (!test || full) && backup::archived(&app, &j) {
        // S3 takes up to 48 hours to thaw the data, longer than a request should wait; the run records how it went.
        tokio::spawn(async move {
            if let Err(e) = backup::restore(&app2, &rid, test, full).await {
                tracing::warn!("restoring backup run {rid} failed: {e:#}");
            }
        });
        let what = if test { "full test restore" } else { "restore" };
        let msg = format!("The {what} started. This backup is in S3 Glacier Deep Archive, so S3 first thaws it, which can take up to 48 hours. It shows up in the job's runs and gets its result there when it finishes.");
        return Ok(Json(json!({ "ok": true, "message": msg })));
    }
    let msg = detached(async move { backup::restore(&app2, &rid, test, full).await }).await?.map_err(bad)?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}

/// The latest backup and restore runs across every job the caller can see, for the Actions page.
pub async fn recent_runs(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let rows = app.db.rows(
        "SELECT r.id, r.job_id, r.kind, r.status, r.size_bytes, r.error, r.started_at, r.finished_at, j.kind AS job_kind, j.service_id, j.volume
         FROM backup_runs r JOIN backup_jobs j ON j.id = r.job_id ORDER BY r.started_at DESC LIMIT 300",
        [],
        row_json,
    )?;
    let mut out = vec![];
    for mut run in rows {
        let service = run["service_id"].as_str().and_then(|sid| crate::store::service(&app, sid).ok().flatten());
        if require_job(&a, &service.as_ref().map(|s| s.project_id.clone()), Role::Viewer).is_err() {
            continue;
        }
        run["service"] = json!(service.as_ref().map(|s| &s.name));
        run["project"] = json!(service.as_ref().map(|s| &s.project));
        with_progress(&mut run);
        out.push(run);
        if out.len() == 100 {
            break;
        }
    }
    Ok(Json(json!({ "runs": out })))
}

/// A running run's latest progress, as `progress` (null when it has none yet or is not running).
fn with_progress(run: &mut Value) {
    run["progress"] = json!(run["id"].as_str().and_then(backup::progress));
}

/// A run with its job, after checking the caller may see it.
fn run_or_404(app: &AppState, a: &Auth, id: &str) -> ApiResult<Value> {
    let mut run = app
        .db
        .row(
            "SELECT r.id, r.job_id, r.kind, r.status, r.snapshot, r.size_bytes, r.error, r.started_at, r.finished_at, j.kind AS job_kind, j.service_id, j.volume
             FROM backup_runs r JOIN backup_jobs j ON j.id = r.job_id WHERE r.id = ?1",
            [id],
            row_json,
        )?
        .ok_or_else(|| ApiError::not_found("backup run not found"))?;
    let service = run["service_id"].as_str().and_then(|sid| crate::store::service(app, sid).ok().flatten());
    require_job(a, &service.as_ref().map(|s| s.project_id.clone()), Role::Viewer)?;
    run["service"] = json!(service.as_ref().map(|s| &s.name));
    run["project"] = json!(service.as_ref().map(|s| &s.project));
    with_progress(&mut run);
    Ok(run)
}

fn read_log(app: &AppState, id: &str) -> Vec<(i64, String)> {
    crate::deploy::read_log_file(&backup::log_path(app, id))
}

pub async fn get_run(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let run = run_or_404(&app, &a, &id)?;
    let log: Vec<Value> = read_log(&app, &id).into_iter().map(|(ts, l)| json!({ "ts": ts, "line": l })).collect();
    Ok(Json(json!({ "run": run, "log": log })))
}

/// The run's log so far, then new lines and progress as they come, until the run finishes.
pub async fn log_stream(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Events> {
    // Subscribe before reading the run and its file so nothing falls in between.
    let rx = app.events.subscribe();
    let run = run_or_404(&app, &a, &id)?;
    let line = |ts: &Value, l: &Value| sse_json("line", &json!({ "ts": ts, "line": l }));
    let mut backlog: Vec<sse::Event> = read_log(&app, &id).into_iter().map(|(ts, l)| line(&json!(ts), &json!(l))).collect();
    if !run["progress"].is_null() {
        backlog.push(sse_json("progress", &run["progress"]));
    }
    let status = run["status"].as_str().unwrap_or("").to_string();
    let tail: BoxStream<'static, sse::Event> = if status != "running" {
        stream::iter([sse::Event::default().event("status").data(status)]).boxed()
    } else {
        crate::util::subscribe(rx)
            .filter_map(move |e| {
                ready(match e.kind.as_str() {
                    _ if e.data["run_id"] != id.as_str() => None,
                    "backup.log" => Some((line(&e.data["ts"], &e.data["line"]), false)),
                    "backup.progress" => Some((sse_json("progress", &e.data["progress"]), false)),
                    "backup" => {
                        let status = e.data["status"].as_str().unwrap_or("");
                        Some((sse::Event::default().event("status").data(status), status != "running"))
                    }
                    _ => None,
                })
            })
            .scan(false, |finished, (ev, fin)| {
                let out = if *finished { None } else { Some(ev) };
                *finished |= fin;
                ready(out)
            })
            .boxed()
    };
    Ok(sse_events(stream::iter(backlog).chain(tail)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Mount, ServiceSpec};

    #[test]
    fn jobs_back_up_only_what_the_service_mounts() {
        let spec = ServiceSpec {
            mounts: vec![
                Mount::Volume { name: "data".into(), target: "/data".into(), readonly: false },
                Mount::Bind { source: "/var/opt/memos".into(), target: "/var/opt/memos".into(), readonly: false },
                Mount::Bind { source: "/srv/a:b".into(), target: "/x".into(), readonly: false },
            ],
            ..Default::default()
        };
        let s = Some(&spec);
        assert_eq!(check_job("volume", Some("data"), s), Ok(()));
        assert_eq!(check_job("volume", Some("/var/opt/memos"), s), Ok(()));
        assert!(check_job("volume", Some("other"), s).is_err());
        for path in ["/var/opt", "/var/opt/memos/", "/etc", "/srv/a:b", "/data"] {
            assert!(check_job("volume", Some(path), s).is_err(), "{path}");
        }
        assert!(check_job("volume", Some(""), s).is_err());
        assert_eq!(check_job("volume", Some("data"), None), Ok(()));
        assert!(check_job("volume", Some("/var/opt/memos"), None).is_err());
        assert!(check_job("database", None, s).is_err());
        assert_eq!(check_job("state", None, None), Ok(()));
        assert!(check_job("state", None, s).is_err());
    }
}
