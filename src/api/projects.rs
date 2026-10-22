//! Projects, members, shared variables, services, env, files, domains and deploy hooks.

use super::{ApiError, ApiResult, Auth, base_url};
use crate::app::AppState;
use crate::auth::Role;
use crate::databases;
use crate::model::{Build, DatabaseSpec, DomainOptions, Engine, Health, HealthKind, Mount, ServiceSpec, Source, Strategy};
use crate::store::{self, Service};
use crate::util;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn project_or_404(app: &AppState, a: &Auth, id: &str, role: Role) -> ApiResult<store::Project> {
    let p = store::project(app, id)?.ok_or_else(|| ApiError::not_found("project not found"))?;
    a.require(&p.id, role)?;
    Ok(p)
}

pub fn service_or_404(app: &AppState, a: &Auth, id: &str, role: Role) -> ApiResult<Service> {
    let s = store::service(app, id)?.ok_or_else(|| ApiError::not_found("service not found"))?;
    a.require(&s.project_id, role)?;
    Ok(s)
}

/// Each visible service's state, from one container listing per server.
async fn states(app: &AppState, services: &[Service]) -> ApiResult<BTreeMap<String, Value>> {
    let mut reps = crate::runtime::all_replicas(app).await?;
    Ok(services.iter().map(|s| (s.id.clone(), crate::runtime::status_of(app, s, reps.remove(&s.id).unwrap_or_default()))).collect())
}

pub async fn list_projects(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let visible = a.0.visible_projects();
    let all = store::projects(&app)?;
    let services: Vec<Service> = store::services(&app, None)?.into_iter().filter(|s| a.0.can(&s.project_id, Role::Viewer)).collect();
    let status = states(&app, &services).await?;
    let out: Vec<Value> = all
        .into_iter()
        .filter(|p| visible.as_ref().is_none_or(|v| v.contains(&p.id)))
        .map(|p| {
            let mine: Vec<&Service> = services.iter().filter(|s| s.project_id == p.id).collect();
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for s in &mine {
                let state = status.get(&s.id).and_then(|v| v["state"].as_str()).unwrap_or("unknown");
                *counts.entry(state.to_string()).or_default() += 1;
            }
            json!({ "id": p.id, "name": p.name, "description": p.description, "created_at": p.created_at,
                    "services": mine.len(), "states": counts, "role": a.0.role_in(&p.id).map(|r| r.as_str()) })
        })
        .collect();
    Ok(Json(json!({ "projects": out })))
}

/// Every service the caller can see, across projects.
pub async fn list_services(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let services: Vec<Service> = store::services(&app, None)?.into_iter().filter(|s| a.0.can(&s.project_id, Role::Viewer)).collect();
    let mut status = states(&app, &services).await?;
    let out: Vec<Value> = services
        .into_iter()
        .map(|s| {
            let st = status.remove(&s.id).unwrap_or_default();
            json!({ "id": s.id, "name": s.name, "project": s.project, "project_id": s.project_id, "kind": s.kind,
                    "state": st["state"], "running": st["running"], "desired": st["desired"] })
        })
        .collect();
    Ok(Json(json!({ "services": out })))
}

#[derive(Deserialize)]
pub struct ProjectBody {
    name: Option<String>,
    description: Option<String>,
}

pub async fn create_project(State(app): State<AppState>, a: Auth, Json(b): Json<ProjectBody>) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.all_projects_role.is_none_or(|r| r < Role::Operator) {
        return Err(ApiError::forbidden("creating projects needs the owner or an operator role on all projects"));
    }
    let name = b.name.unwrap_or_default().trim().to_lowercase();
    if !util::valid_name(&name) {
        return Err(ApiError::bad_request("project names are lowercase letters, digits and dashes, starting with a letter"));
    }
    let id = util::id();
    app.db.conn().execute(
        "INSERT INTO projects (id, name, description, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, b.description, util::now()],
    )?;
    app.audit(Some(&a.0), "project.create", Some(&id), None, Some(&name), None);
    Ok(Json(json!({ "id": id, "name": name })))
}

pub async fn get_project(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    let list = store::services(&app, Some(&p.id))?;
    let mut status = states(&app, &list).await?;
    let mut services = vec![];
    for s in list {
        let status = status.remove(&s.id).unwrap_or_default();
        services.push(json!({ "id": s.id, "name": s.name, "kind": s.kind, "source": s.spec.source, "status": status }));
    }
    let vars: Vec<String> = store::project_vars(&app, &p.id)?.into_keys().collect();
    Ok(Json(json!({ "project": p, "services": services, "vars": vars, "role": a.0.role_in(&p.id).map(|r| r.as_str()) })))
}

pub async fn update_project(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<ProjectBody>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Admin)?;
    if let Some(d) = &b.description {
        app.db.conn().execute("UPDATE projects SET description = ?1 WHERE id = ?2", params![d, p.id])?;
    }
    if b.name.is_some() {
        return Err(ApiError::bad_request("projects cannot be renamed because names are part of container and network names"));
    }
    app.audit(Some(&a.0), "project.update", Some(&p.id), None, Some(&p.name), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_project(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    if !a.0.can_destroy(&p.id) {
        return Err(ApiError::forbidden("deleting needs the admin role and the destroy grant"));
    }
    if !store::services(&app, Some(&p.id))?.is_empty() {
        return Err(ApiError::conflict("delete the project's services first"));
    }
    app.db.conn().execute("DELETE FROM projects WHERE id = ?1", [&p.id])?;
    crate::runtime::remove_project_network(&app, &p).await;
    app.audit(Some(&a.0), "project.delete", Some(&p.id), None, Some(&p.name), None);
    Ok(Json(json!({ "ok": true })))
}

/// Names, plus values only for callers allowed to read secrets.
fn reveal(map: BTreeMap<String, zeroize::Zeroizing<String>>, can_read: bool) -> Value {
    if can_read {
        json!(map.into_iter().map(|(k, v)| (k, v.to_string())).collect::<BTreeMap<_, _>>())
    } else {
        json!(map.into_keys().collect::<Vec<_>>())
    }
}

pub async fn get_vars(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    let can = a.0.can_read_secrets(&p.id);
    Ok(Json(json!({ "vars": reveal(store::project_vars(&app, &p.id)?, can), "values_visible": can })))
}

pub async fn put_vars(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<BTreeMap<String, Option<String>>>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Operator)?;
    for (k, v) in &b {
        if !store::valid_env_key(k) {
            return Err(ApiError::bad_request(format!("{k} is not a valid variable name")));
        }
        store::set_project_var(&app, &p.id, k, v.as_deref())?;
    }
    let keys: Vec<&str> = b.keys().map(|k| k.as_str()).collect();
    app.audit(Some(&a.0), "project.vars", Some(&p.id), None, None, Some(&keys.join(",")));
    Ok(Json(json!({ "ok": true, "redeploy_needed": true })))
}

pub async fn list_members(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    let conn = app.db.conn();
    let mut stmt = conn.prepare(
        "SELECT u.id, u.name, u.kind, COALESCE(pa.role, u.all_projects_role), pa.role IS NULL, u.is_owner FROM users u
         LEFT JOIN project_access pa ON pa.user_id = u.id AND pa.project_id = ?1
         WHERE pa.role IS NOT NULL OR u.all_projects_role IS NOT NULL OR u.is_owner = 1 ORDER BY u.name",
    )?;
    let members: Vec<Value> = stmt
        .query_map([&p.id], |r| {
            Ok(json!({ "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?,
                       "role": if r.get::<_, bool>(5)? { Some("owner".to_string()) } else { r.get::<_, Option<String>>(3)? },
                       "via_all_projects": r.get::<_, bool>(4)? }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "members": members })))
}

#[derive(Deserialize)]
pub struct MemberBody {
    role: Option<Role>,
}

pub async fn set_member(State(app): State<AppState>, a: Auth, Path((id, user)): Path<(String, String)>, Json(b): Json<MemberBody>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Admin)?;
    let name: String = app
        .db
        .conn()
        .query_row("SELECT name FROM users WHERE id = ?1 OR name = ?1", [&user], |r| r.get(0))
        .optional()?
        .ok_or_else(|| ApiError::not_found("no such user"))?;
    match b.role {
        Some(role) => app.db.conn().execute(
            "INSERT INTO project_access (user_id, project_id, role) SELECT id, ?2, ?3 FROM users WHERE id = ?1 OR name = ?1
             ON CONFLICT(user_id, project_id) DO UPDATE SET role = excluded.role",
            params![user, p.id, role.as_str()],
        )?,
        None => app.db.conn().execute(
            "DELETE FROM project_access WHERE project_id = ?2 AND user_id IN (SELECT id FROM users WHERE id = ?1 OR name = ?1)",
            params![user, p.id],
        )?,
    };
    app.audit(Some(&a.0), "project.member", Some(&p.id), None, Some(&name), b.role.map(|r| r.as_str()));
    Ok(Json(json!({ "ok": true })))
}

/// Settings that can hand out host-level access need the destroy grant (or the owner).
fn check_dangerous(a: &Auth, project_id: &str, old: Option<&ServiceSpec>, new: &ServiceSpec, paths: &crate::paths::Paths) -> ApiResult<()> {
    let binds = |s: &ServiceSpec| -> Vec<String> {
        s.mounts.iter().filter_map(|m| if let Mount::Bind { source, .. } = m { Some(source.clone()) } else { None }).collect()
    };
    for m in &new.mounts {
        if let Mount::Bind { source, .. } = m {
            let src = std::path::Path::new(source);
            if !src.is_absolute() {
                return Err(ApiError::bad_request(format!("bind source {source} must be an absolute path")));
            }
            let forbidden = [&paths.etc, &paths.data, &paths.run];
            if forbidden.iter().any(|f| src.starts_with(f)) {
                return Err(ApiError::bad_request("services cannot mount Tugport's own files"));
            }
            if source.ends_with("docker.sock") && !new.docker_socket {
                return Err(ApiError::bad_request("use the docker_socket switch to give a service the Docker socket"));
            }
        }
    }
    let changed = |f: fn(&ServiceSpec) -> bool| f(new) && old.is_none_or(|o| !f(o));
    let new_binds = binds(new).into_iter().any(|b| old.is_none_or(|o| !binds(o).contains(&b)));
    let new_caps = new.cap_add.iter().any(|c| old.is_none_or(|o| !o.cap_add.contains(c)));
    if (changed(|s| s.privileged) || changed(|s| s.docker_socket) || changed(|s| s.host_network) || new_binds || new_caps)
        && !a.0.can_destroy(project_id)
    {
        return Err(ApiError::forbidden(
            "privileged mode, the Docker socket, host networking, bind mounts and extra capabilities need the admin role and the destroy grant",
        ));
    }
    if new.replicas > 100 {
        return Err(ApiError::bad_request("at most 100 replicas"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateService {
    name: String,
    #[serde(default = "app_kind")]
    kind: String,
    #[serde(default)]
    spec: Option<Value>,
    /// Shortcut for databases: `{"engine": "postgres"}`.
    #[serde(default)]
    database: Option<DatabaseSpec>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn app_kind() -> String {
    "app".into()
}

pub async fn create_service(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(project): Path<String>, Json(b): Json<CreateService>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &project, Role::Operator)?;
    let name = b.name.trim().to_lowercase();
    if !util::valid_name(&name) {
        return Err(ApiError::bad_request("service names are lowercase letters, digits and dashes, starting with a letter"));
    }
    let mut spec: ServiceSpec = match b.spec {
        Some(v) => serde_json::from_value(v).map_err(|e| ApiError::bad_request(format!("invalid spec: {e}")))?,
        None => ServiceSpec::default(),
    };
    let mut env = b.env;
    let mut reveal_once = None;
    match b.kind.as_str() {
        "app" => {
            if matches!(spec.source, Source::Compose { .. }) {
                return Err(ApiError::bad_request("use kind \"compose\" for Compose stacks"));
            }
            if let Source::Image { image } = &spec.source
                && image.trim().is_empty() {
                    return Err(ApiError::bad_request("pick an image or a repository"));
                }
        }
        "compose" => {
            spec.compose.get_or_insert_with(Default::default);
            if !matches!(spec.source, Source::Compose { .. } | Source::Git { .. }) {
                return Err(ApiError::bad_request("Compose stacks come from pasted YAML or a Git repository"));
            }
        }
        "database" => {
            let db = b.database.or(spec.database.take()).ok_or_else(|| ApiError::bad_request("pick a database engine"))?;
            let (url, url_var) = prepare_database(&app, &p, &name, db, &mut spec, &mut env).await?;
            if !store::project_vars(&app, &p.id)?.contains_key(&url_var) {
                store::set_project_var(&app, &p.id, &url_var, Some(&url))?;
            }
            reveal_once = Some(json!({ "url": url, "project_var": url_var }));
        }
        other => return Err(ApiError::bad_request(format!("unknown kind {other}; use app, database or compose"))),
    }
    check_dangerous(&a, &p.id, None, &spec, &app.paths)?;
    for k in env.keys() {
        if !store::valid_env_key(k) {
            return Err(ApiError::bad_request(format!("{k} is not a valid variable name")));
        }
    }
    let id = util::id();
    let now = util::now();
    let spec_json = serde_json::to_string(&spec)?;
    app.db.conn().execute(
        "INSERT INTO services (id, project_id, name, kind, spec, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, p.id, name, b.kind, spec_json, now],
    )?;
    for (k, v) in &env {
        store::set_env(&app, &id, k, Some(v))?;
    }
    app.audit(Some(&a.0), "service.create", Some(&p.id), Some(&id), Some(&name), Some(&b.kind));
    let hook = new_hook_url(&app, &id, &headers)?;
    if let Some(created) = store::service(&app, &id)? {
        let (app2, base) = (app.clone(), super::git::public_base(&app, &headers));
        tokio::spawn(async move { super::git::ensure_hook(&app2, &created, &base).await });
    }
    Ok(Json(json!({ "id": id, "name": name, "kind": b.kind, "spec": spec, "deploy_hook": hook, "database": reveal_once })))
}

async fn prepare_database(
    _app: &AppState,
    p: &store::Project,
    name: &str,
    mut db: DatabaseSpec,
    spec: &mut ServiceSpec,
    env: &mut BTreeMap<String, String>,
) -> ApiResult<(String, String)> {
    let preset = databases::preset(db.engine);
    let image = match &spec.source {
        Source::Image { image } if !image.trim().is_empty() => image.clone(),
        _ => format!("{}:{}", preset.repo, databases::latest_version(db.engine).await),
    };
    let version = image.rsplit_once(':').map(|(_, v)| v.to_string()).unwrap_or_default();
    if db.user.is_empty() {
        db.user = match db.engine {
            Engine::Mongo => "admin".into(),
            Engine::Redis => "default".into(),
            _ => "app".into(),
        };
    }
    if db.database.is_empty() {
        db.database = "app".into();
    }
    let password = util::token(32);
    for (k, v) in databases::init_env(db.engine, &db.user, &db.database, &password) {
        env.entry(k.into()).or_insert(v);
    }
    let volume = format!("tp-{}-{name}-data", p.name);
    spec.source = Source::Image { image };
    spec.build = Build::Auto;
    spec.command = databases::command(db.engine);
    spec.mounts.retain(|m| m.target() != databases::data_dir(db.engine, &version));
    spec.mounts.push(Mount::Volume { name: volume, target: databases::data_dir(db.engine, &version).into(), readonly: false });
    spec.health = Health { kind: HealthKind::Command, command: Some(databases::health(db.engine).into()), start_period_s: 30, ..Health::default() };
    // Two copies of a database must never share a data directory.
    spec.rollout.strategy = Strategy::StopFirst;
    spec.replicas = 1;
    let url = databases::url(db.engine, name, &db.user, &password, &db.database);
    let url_var = format!("{}_URL", name.to_uppercase().replace('-', "_"));
    spec.database = Some(db);
    Ok((url, url_var))
}

pub async fn get_service(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let status = crate::runtime::service_status(&app, &s).await;
    let domains = store::domains(&app, Some(&s.id))?;
    let env = store::env_names(&app, &s.id)?;
    let files: Vec<String> = store::files(&app, &s.id)?.into_iter().map(|(t, _)| t).collect();
    let last = crate::deploy::recent(&app, &s.id, 1)?.into_iter().next();
    // Ports the running image declares (EXPOSE), to suggest a domain's port.
    let exposed: Vec<u16> = app.db.get_state(&format!("exposed:{}", s.id))?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default();
    Ok(Json(json!({
        "service": s, "status": status, "domains": domains, "env": env, "files": files, "last_deploy": last,
        "exposed_ports": exposed, "role": a.0.role_in(&s.project_id).map(|r| r.as_str()),
    })))
}

#[derive(Deserialize)]
pub struct UpdateService {
    spec: Option<Value>,
}

pub async fn update_service(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>, Json(b): Json<UpdateService>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    let Some(patch) = b.spec else { return Ok(Json(json!({ "spec": s.spec }))) };
    let mut current = serde_json::to_value(&s.spec)?;
    super::access::merge(&mut current, &patch);
    let next: ServiceSpec = serde_json::from_value(current).map_err(|e| ApiError::bad_request(format!("invalid spec: {e}")))?;
    check_dangerous(&a, &s.project_id, Some(&s.spec), &next, &app.paths)?;
    if s.kind == "database" && next.replicas > 1 {
        return Err(ApiError::bad_request("databases run one primary; add read replicas instead"));
    }
    store::save_spec(&app, &s.id, &next)?;
    let fields: Vec<&str> = patch.as_object().map(|o| o.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
    app.audit(Some(&a.0), "service.update", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&fields.join(",")));
    app.emit("service.updated", Some(&s.project_id), Some(&s.id), json!({}));
    if let Some(updated) = store::service(&app, &s.id)? {
        let (app2, base) = (app.clone(), super::git::public_base(&app, &headers));
        tokio::spawn(async move { super::git::ensure_hook(&app2, &updated, &base).await });
    }
    Ok(Json(json!({ "spec": next, "redeploy_needed": true })))
}

#[derive(Deserialize, Default)]
pub struct DeleteQuery {
    #[serde(default)]
    volumes: bool,
}

pub async fn delete_service(
    State(app): State<AppState>,
    a: Auth,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DeleteQuery>,
) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    if !a.0.can_destroy(&s.project_id) {
        return Err(ApiError::forbidden("deleting needs the admin role and the destroy grant"));
    }
    if !crate::replicas::of(&app, &s.id)?.is_empty() {
        return Err(ApiError::conflict("this database has replicas; promote or delete them first"));
    }
    crate::deploy::cancel_all(&app, &s.id);
    crate::runtime::remove_service(&app, &s, q.volumes).await?;
    app.db.conn().execute("DELETE FROM services WHERE id = ?1", [&s.id])?;
    app.audit(Some(&a.0), "service.delete", Some(&s.project_id), Some(&s.id), Some(&s.name), q.volumes.then_some("with volumes"));
    crate::proxy::reload(&app);
    Ok(Json(json!({ "ok": true })))
}

pub async fn get_env(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let can = a.0.can_read_secrets(&s.project_id);
    let project: Vec<String> = store::project_vars(&app, &s.project_id)?.into_keys().collect();
    Ok(Json(json!({ "env": reveal(store::service_env(&app, &s.id)?, can), "project_vars": project, "values_visible": can })))
}

pub async fn put_env(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<BTreeMap<String, Option<String>>>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    for k in b.keys() {
        if !store::valid_env_key(k) {
            return Err(ApiError::bad_request(format!("{k} is not a valid variable name")));
        }
    }
    for (k, v) in &b {
        store::set_env(&app, &s.id, k, v.as_deref())?;
    }
    let keys: Vec<&str> = b.keys().map(|k| k.as_str()).collect();
    app.audit(Some(&a.0), "service.env", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&keys.join(",")));
    Ok(Json(json!({ "ok": true, "redeploy_needed": true })))
}

pub async fn list_files(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let can = a.0.can_read_secrets(&s.project_id);
    let files: Vec<Value> = store::files(&app, &s.id)?
        .into_iter()
        .map(|(t, c)| if can { json!({ "target": t, "content": c.to_string() }) } else { json!({ "target": t }) })
        .collect();
    Ok(Json(json!({ "files": files, "values_visible": can })))
}

#[derive(Deserialize)]
pub struct FileBody {
    target: String,
    content: Option<String>,
}

pub async fn put_file(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<FileBody>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if !b.target.starts_with('/') || b.target.contains("..") {
        return Err(ApiError::bad_request("file targets are absolute paths inside the container"));
    }
    store::set_file(&app, &s.id, &b.target, b.content.as_deref())?;
    let mut spec = s.spec.clone();
    spec.mounts.retain(|m| !matches!(m, Mount::File { target } if *target == b.target));
    if b.content.is_some() {
        spec.mounts.push(Mount::File { target: b.target.clone() });
    }
    store::save_spec(&app, &s.id, &spec)?;
    app.audit(Some(&a.0), "service.file", Some(&s.project_id), Some(&s.id), Some(&b.target), Some(if b.content.is_some() { "set" } else { "removed" }));
    Ok(Json(json!({ "ok": true, "redeploy_needed": true })))
}

#[derive(Deserialize)]
pub struct DomainBody {
    host: Option<String>,
    path: Option<String>,
    port: Option<u16>,
    /// Compose container the domain points at; null clears it.
    #[serde(default, deserialize_with = "super::present")]
    container: Option<Option<String>>,
    https: Option<bool>,
    via_tunnel: Option<bool>,
    options: Option<DomainOptions>,
}

pub async fn list_domains(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    Ok(Json(json!({ "domains": store::domains(&app, Some(&s.id))? })))
}

/// New basic-auth passwords become Argon2 hashes; users sent without one keep theirs.
fn hash_basic_auth(mut opts: DomainOptions, previous: Option<&DomainOptions>) -> ApiResult<DomainOptions> {
    for u in &mut opts.basic_auth {
        match u.password.take().filter(|p| !p.is_empty()) {
            Some(p) => u.hash = crate::auth::hash_password(&p)?,
            None if !u.hash.is_empty() => {}
            None => {
                let kept = previous.and_then(|p| p.basic_auth.iter().find(|o| o.user == u.user)).map(|o| o.hash.clone());
                u.hash = kept.ok_or_else(|| ApiError::bad_request(format!("set a password for {}", u.user)))?;
            }
        }
    }
    Ok(opts)
}

fn validate_domain(host: &str, path: &str, port: u16, opts: &DomainOptions) -> ApiResult<()> {
    if !store::valid_host(host) {
        return Err(ApiError::bad_request(format!("{host} is not a valid domain")));
    }
    if !path.starts_with('/') {
        return Err(ApiError::bad_request("paths start with /"));
    }
    if port == 0 {
        return Err(ApiError::bad_request("pick the container port the service listens on"));
    }
    for r in &opts.redirects {
        regex::Regex::new(&r.from).map_err(|e| ApiError::bad_request(format!("redirect pattern: {e}")))?;
    }
    if let Some(rw) = &opts.rewrite {
        regex::Regex::new(&rw.from).map_err(|e| ApiError::bad_request(format!("rewrite pattern: {e}")))?;
    }
    Ok(())
}

pub async fn create_domain(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<DomainBody>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    let host = b.host.unwrap_or_default().trim().to_lowercase();
    let path = b.path.unwrap_or_else(|| "/".into());
    let port = b.port.or_else(|| s.spec.database.as_ref().map(|d| databases::preset(d.engine).port)).unwrap_or(0);
    let opts = hash_basic_auth(b.options.unwrap_or_default(), None)?;
    validate_domain(&host, &path, port, &opts)?;
    let did = util::id();
    let opts_json = serde_json::to_string(&opts)?;
    app.db.conn().execute(
        "INSERT INTO domains (id, service_id, host, path, port, container, https, via_tunnel, options, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![did, s.id, host, path, port, b.container.flatten(), b.https.unwrap_or(true), b.via_tunnel.unwrap_or(false), opts_json, util::now()],
    )?;
    app.audit(Some(&a.0), "domain.create", Some(&s.project_id), Some(&s.id), Some(&host), None);
    crate::proxy::reload(&app);
    crate::tunnel::sync_soon(&app);
    Ok(Json(json!({ "domain": store::domain(&app, &did)? })))
}

pub async fn update_domain(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<DomainBody>) -> ApiResult<Json<Value>> {
    let d = store::domain(&app, &id)?.ok_or_else(|| ApiError::not_found("domain not found"))?;
    let s = service_or_404(&app, &a, &d.service_id, Role::Operator)?;
    let host = b.host.map(|h| h.trim().to_lowercase()).unwrap_or(d.host);
    let path = b.path.unwrap_or(d.path);
    let port = b.port.unwrap_or(d.port);
    let opts = match b.options {
        Some(o) => hash_basic_auth(o, Some(&d.options))?,
        None => d.options,
    };
    validate_domain(&host, &path, port, &opts)?;
    let opts_json = serde_json::to_string(&opts)?;
    app.db.conn().execute(
        "UPDATE domains SET host = ?1, path = ?2, port = ?3, container = ?4, https = ?5, via_tunnel = ?6, options = ?7 WHERE id = ?8",
        params![host, path, port, b.container.unwrap_or(d.container), b.https.unwrap_or(d.https), b.via_tunnel.unwrap_or(d.via_tunnel), opts_json, id],
    )?;
    app.audit(Some(&a.0), "domain.update", Some(&s.project_id), Some(&s.id), Some(&host), None);
    crate::proxy::reload(&app);
    crate::tunnel::sync_soon(&app);
    Ok(Json(json!({ "domain": store::domain(&app, &id)? })))
}

pub async fn delete_domain(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let d = store::domain(&app, &id)?.ok_or_else(|| ApiError::not_found("domain not found"))?;
    let s = service_or_404(&app, &a, &d.service_id, Role::Operator)?;
    app.db.conn().execute("DELETE FROM domains WHERE id = ?1", [&id])?;
    app.audit(Some(&a.0), "domain.delete", Some(&s.project_id), Some(&s.id), Some(&d.host), None);
    crate::proxy::reload(&app);
    crate::tunnel::sync_soon(&app);
    Ok(Json(json!({ "ok": true })))
}

fn new_hook_url(app: &AppState, service_id: &str, headers: &HeaderMap) -> ApiResult<String> {
    let token = util::token(32);
    app.db.conn().execute("UPDATE services SET deploy_token_hash = ?1 WHERE id = ?2", params![util::sha256_hex(&token), service_id])?;
    // Kept sealed too, so people allowed to read secrets can see the URL again.
    let key = format!("hook:{service_id}");
    app.db.set_state(&key, &crate::crypto::b64(&app.keys.seal_str(&key, &token)?))?;
    Ok(format!("{}/api/hooks/deploy/{token}", base_url(headers)))
}

/// The current deploy webhook URL (it deploys the service, so it needs secrets:read).
pub async fn get_hook(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if !a.0.can_read_secrets(&s.project_id) {
        return Err(ApiError::forbidden("the webhook URL is a secret; it needs the secrets:read grant"));
    }
    let key = format!("hook:{}", s.id);
    let sealed = app.db.get_state(&key)?.ok_or_else(|| ApiError::not_found("this webhook was made before URLs were kept; rotate it to get a new one"))?;
    let token = app.keys.open_str(&key, &crate::crypto::unb64(&sealed)?)?;
    Ok(Json(json!({ "url": format!("{}/api/hooks/deploy/{}", base_url(&headers), token.as_str()) })))
}

/// Replace the service's deploy webhook URL (the old one stops working).
pub async fn rotate_hook(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    let url = new_hook_url(&app, &s.id, &headers)?;
    app.audit(Some(&a.0), "service.hook_rotated", Some(&s.project_id), Some(&s.id), Some(&s.name), None);
    Ok(Json(json!({ "url": url })))
}
