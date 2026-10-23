//! Backup targets (S3 or local), jobs, runs and restores.

use super::projects::service_or_404;
use super::{ApiError, ApiResult, Auth};
use crate::app::AppState;
use crate::auth::Role;
use crate::{backup, util};
use axum::Json;
use axum::extract::{Path, State};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

pub async fn list_targets(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.all_projects_role.is_none() {
        return Err(ApiError::forbidden("needs the owner or a role on all projects"));
    }
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, name, kind, created_at FROM backup_targets ORDER BY created_at")?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| Ok(json!({ "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "created_at": r.get::<_, i64>(3)? })))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "targets": rows })))
}

#[derive(Deserialize)]
pub struct TargetBody {
    name: String,
    kind: String,
    config: Value,
}

pub async fn create_target(State(app): State<AppState>, a: Auth, Json(b): Json<TargetBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
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
    if b.kind == "local" && !b.config["path"].as_str().unwrap_or("").starts_with('/') {
        return Err(ApiError::bad_request("local targets need an absolute path"));
    }
    let id = util::id();
    let sealed = app.keys.seal_str(&format!("backup-target:{id}"), &b.config.to_string())?;
    app.db.conn().execute(
        "INSERT INTO backup_targets (id, name, kind, config, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, b.name, b.kind, sealed, util::now()],
    )?;
    app.audit(Some(&a.0), "backup.target_create", None, None, Some(&b.name), Some(&b.kind));
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_target(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    app.db.conn().execute("DELETE FROM backup_targets WHERE id = ?1", [&id])?;
    app.audit(Some(&a.0), "backup.target_delete", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}

fn job_project(app: &AppState, job_id: &str) -> ApiResult<(backup::Job, Option<String>)> {
    let j = backup::job(app, job_id).map_err(|_| ApiError::not_found("backup job not found"))?;
    let project = match &j.service_id {
        Some(sid) => crate::store::service(app, sid)?.map(|s| s.project_id),
        None => None,
    };
    Ok((j, project))
}

fn require_job(a: &Auth, project: &Option<String>, role: Role) -> ApiResult<()> {
    match project {
        Some(p) => a.require(p, role),
        None => a.require_owner(),
    }
}

#[allow(clippy::type_complexity)]
pub async fn list_jobs(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let rows: Vec<(String, Option<String>, String, String, Option<String>, String, i64, bool)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT id, service_id, target_id, kind, volume, schedule, keep_last, enabled FROM backup_jobs ORDER BY created_at")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.filter_map(|r| r.ok()).collect()
    };
    let mut out = vec![];
    for (id, service_id, target_id, kind, volume, schedule, keep_last, enabled) in rows {
        let project = match &service_id {
            Some(sid) => crate::store::service(&app, sid)?.map(|s| s.project_id),
            None => None,
        };
        let visible = match &project {
            Some(p) => a.0.can(p, Role::Viewer),
            None => a.0.is_owner,
        };
        if !visible {
            continue;
        }
        let last: Option<Value> = app
            .db
            .conn()
            .query_row(
                "SELECT status, finished_at, error FROM backup_runs WHERE job_id = ?1 AND kind = 'backup' ORDER BY started_at DESC LIMIT 1",
                [&id],
                |r| Ok(json!({ "status": r.get::<_, String>(0)?, "finished_at": r.get::<_, Option<i64>>(1)?, "error": r.get::<_, Option<String>>(2)? })),
            )
            .ok();
        out.push(json!({ "id": id, "service_id": service_id, "target_id": target_id, "kind": kind, "volume": volume,
                         "schedule": schedule, "keep_last": keep_last, "enabled": enabled, "last_run": last }));
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
    match (b.kind.as_str(), &service) {
        ("database", Some(s)) if s.spec.database.is_some() => {}
        ("database", _) => return Err(ApiError::bad_request("database backups need a database service")),
        ("volume", _) if b.volume.as_deref().is_none_or(|v| v.is_empty()) => return Err(ApiError::bad_request("volume backups need a volume name")),
        // The name reaches `docker run -v`, where a path would mean a folder of the server, and
        // another service's volume another project's data.
        ("volume", Some(s))
            if !s.spec.mounts.iter().any(|m| matches!(m, crate::model::Mount::Volume { name, .. } if Some(name.as_str()) == b.volume.as_deref())) =>
        {
            return Err(ApiError::bad_request("pick one of the service's volumes"));
        }
        ("volume", None) if b.volume.as_deref().is_some_and(|v| v.contains(['/', ':'])) => return Err(ApiError::bad_request("that is not a volume name")),
        ("volume", _) | ("state", None) => {}
        ("state", Some(_)) => return Err(ApiError::bad_request("state backups are not tied to a service")),
        _ => return Err(ApiError::bad_request("kind is database, volume or state")),
    }
    backup::target(&app, &b.target_id).map_err(|_| ApiError::bad_request("unknown backup target"))?;
    let id = util::id();
    app.db.conn().execute(
        "INSERT INTO backup_jobs (id, service_id, target_id, kind, volume, schedule, keep_last, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, service.as_ref().map(|s| s.id.clone()), b.target_id, b.kind, b.volume, b.schedule, b.keep_last.max(1), util::now()],
    )?;
    app.audit(Some(&a.0), "backup.job_create", service.as_ref().map(|s| s.project_id.as_str()), service.as_ref().map(|s| s.id.as_str()), Some(&b.kind), Some(&b.schedule));
    Ok(Json(json!({ "id": id })))
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
    app.db.conn().execute(
        "UPDATE backup_jobs SET schedule = COALESCE(?1, schedule), keep_last = COALESCE(?2, keep_last), enabled = COALESCE(?3, enabled) WHERE id = ?4",
        params![b.schedule, b.keep_last.map(|k| k.max(1)), b.enabled, j.id],
    )?;
    app.audit(Some(&a.0), "backup.job_update", project.as_deref(), j.service_id.as_deref(), Some(&j.kind), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_job(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Admin)?;
    app.db.conn().execute("DELETE FROM backup_jobs WHERE id = ?1", [&j.id])?;
    app.audit(Some(&a.0), "backup.job_delete", project.as_deref(), j.service_id.as_deref(), Some(&j.kind), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn run_job(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Operator)?;
    app.audit(Some(&a.0), "backup.run", project.as_deref(), j.service_id.as_deref(), Some(&j.kind), None);
    // The run finishes even if the caller goes away.
    let (app2, jid) = (app.clone(), j.id.clone());
    let run = tokio::spawn(async move { backup::run_job(&app2, &jid).await }).await.map_err(anyhow::Error::from)?;
    let run = run.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "run_id": run })))
}

pub async fn list_runs(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (j, project) = job_project(&app, &id)?;
    require_job(&a, &project, Role::Viewer)?;
    let conn = app.db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, kind, status, snapshot, size_bytes, error, started_at, finished_at FROM backup_runs WHERE job_id = ?1 ORDER BY started_at DESC LIMIT 100",
    )?;
    let runs: Vec<Value> = stmt
        .query_map([&j.id], |r| {
            Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "status": r.get::<_, String>(2)?,
                       "snapshot": r.get::<_, Option<String>>(3)?, "size_bytes": r.get::<_, Option<i64>>(4)?,
                       "error": r.get::<_, Option<String>>(5)?, "started_at": r.get::<_, i64>(6)?, "finished_at": r.get::<_, Option<i64>>(7)? }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "runs": runs })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RestoreBody {
    test: bool,
}

pub async fn restore(State(app): State<AppState>, a: Auth, Path(run_id): Path<String>, body: Option<Json<RestoreBody>>) -> ApiResult<Json<Value>> {
    let test = body.map(|b| b.0.test).unwrap_or(false);
    let job_id: String = app
        .db
        .conn()
        .query_row("SELECT job_id FROM backup_runs WHERE id = ?1", [&run_id], |r| r.get(0))
        .map_err(|_| ApiError::not_found("backup run not found"))?;
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
    let msg = tokio::spawn(async move { backup::restore(&app2, &rid, test).await }).await.map_err(anyhow::Error::from)?;
    let msg = msg.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true, "message": msg })))
}
