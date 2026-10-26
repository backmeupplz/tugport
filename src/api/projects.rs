//! Projects, members, shared variables, services, env, files, domains and deploy hooks.

use super::service_or_404;
use super::{ApiError, ApiResult, Auth, base_url, names, ok, project_or_404, visible_services};
use crate::app::AppState;
use crate::auth::Role;
use crate::databases;
use crate::model::{Build, DatabaseSpec, DomainOptions, Engine, Health, Mount, ServiceSpec, Source, Strategy};
use crate::store::{self, Domain, Service};
use crate::util;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Each service's state, from the proxy's latest container listing.
async fn states(app: &AppState, services: &[Service]) -> ApiResult<BTreeMap<String, Value>> {
    let reps = crate::runtime::known_replicas(app).await?;
    Ok(services.iter().map(|s| (s.id.clone(), crate::runtime::status_of(app, s, reps.get(&s.id).cloned().unwrap_or_default()))).collect())
}

fn role(a: &Auth, project_id: &str) -> Option<&'static str> {
    a.0.role_in(project_id).map(|r| r.as_str())
}

fn require_destroy(a: &Auth, project_id: &str) -> ApiResult<()> {
    if a.0.can_destroy(project_id) { Ok(()) } else { Err(ApiError::forbidden("deleting needs the admin role and the destroy grant")) }
}

fn check_env_keys<'a>(keys: impl IntoIterator<Item = &'a String>) -> ApiResult<()> {
    match keys.into_iter().find(|k| !store::valid_env_key(k)) {
        Some(k) => Err(ApiError::bad_request(format!("{k} is not a valid variable name"))),
        None => Ok(()),
    }
}

fn redeploy_needed() -> Json<Value> {
    Json(json!({ "ok": true, "redeploy_needed": true }))
}

pub async fn list_projects(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let visible = a.0.visible_projects();
    let services = visible_services(&app, &a)?;
    let status = states(&app, &services).await?;
    let out: Vec<Value> = store::projects(&app)?
        .into_iter()
        .filter(|p| visible.as_ref().is_none_or(|v| v.contains(&p.id)))
        .map(|p| {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            let mine = services.iter().filter(|s| s.project_id == p.id);
            for s in mine.clone() {
                *counts.entry(status.get(&s.id).and_then(|v| v["state"].as_str()).unwrap_or("unknown")).or_default() += 1;
            }
            json!({ "id": p.id, "name": p.name, "description": p.description, "created_at": p.created_at,
                    "services": mine.count(), "states": counts, "role": role(&a, &p.id) })
        })
        .collect();
    Ok(Json(json!({ "projects": out })))
}

/// Every service the caller can see, across projects.
pub async fn list_services(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let services = visible_services(&app, &a)?;
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
    if !a.operates_all_projects() {
        return Err(ApiError::forbidden("creating projects needs the owner or an operator role on all projects"));
    }
    let name = b.name.unwrap_or_default().trim().to_lowercase();
    if !util::valid_name(&name) {
        return Err(ApiError::bad_request("project names are lowercase letters, digits and dashes, starting with a letter"));
    }
    // Its network would be tp-ingress, the shared bridge every proxy reaches replicas through.
    if name == "ingress" {
        return Err(ApiError::bad_request("ingress is a name Tugport uses itself; pick another"));
    }
    let id = util::id();
    app.db.exec("INSERT INTO projects (id, name, description, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, name, b.description, util::now()])?;
    app.audit(Some(&a.0), "project.create", Some(&id), None, Some(&name), None);
    Ok(Json(json!({ "id": id, "name": name })))
}

pub async fn get_project(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    let list = store::services(&app, Some(&p.id))?;
    let mut status = states(&app, &list).await?;
    let services: Vec<Value> = list
        .into_iter()
        .map(|s| json!({ "id": s.id, "name": s.name, "kind": s.kind, "source": s.spec.source, "status": status.remove(&s.id).unwrap_or_default() }))
        .collect();
    let vars: Vec<String> = store::project_vars(&app, &p.id)?.into_keys().collect();
    Ok(Json(json!({ "project": p, "services": services, "vars": vars, "role": role(&a, &p.id) })))
}

pub async fn update_project(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<ProjectBody>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Admin)?;
    if let Some(d) = &b.description {
        app.db.exec("UPDATE projects SET description = ?1 WHERE id = ?2", params![d, p.id])?;
    }
    if b.name.is_some() {
        return Err(ApiError::bad_request("projects cannot be renamed because names are part of container and network names"));
    }
    app.audit(Some(&a.0), "project.update", Some(&p.id), None, Some(&p.name), None);
    Ok(ok())
}

pub async fn delete_project(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    require_destroy(&a, &p.id)?;
    if !store::services(&app, Some(&p.id))?.is_empty() {
        return Err(ApiError::conflict("delete the project's services first"));
    }
    app.db.exec("DELETE FROM projects WHERE id = ?1", [&p.id])?;
    crate::runtime::remove_project_network(&app, &p).await;
    app.audit(Some(&a.0), "project.delete", Some(&p.id), None, Some(&p.name), None);
    Ok(ok())
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
    check_env_keys(b.keys())?;
    for (k, v) in &b {
        store::set_project_var(&app, &p.id, k, v.as_deref())?;
    }
    app.audit(Some(&a.0), "project.vars", Some(&p.id), None, None, Some(&names(b.keys())));
    Ok(redeploy_needed())
}

pub async fn list_members(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Viewer)?;
    let members = app.db.rows(
        "SELECT u.id, u.name, u.kind, COALESCE(pa.role, u.all_projects_role), pa.role IS NULL, u.is_owner FROM users u
         LEFT JOIN project_access pa ON pa.user_id = u.id AND pa.project_id = ?1
         WHERE pa.role IS NOT NULL OR u.all_projects_role IS NOT NULL OR u.is_owner = 1 ORDER BY u.name",
        [&p.id],
        |r| {
            Ok(json!({ "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?,
                       "role": if r.get::<_, bool>(5)? { Some("owner".to_string()) } else { r.get::<_, Option<String>>(3)? },
                       "via_all_projects": r.get::<_, bool>(4)? }))
        },
    )?;
    Ok(Json(json!({ "members": members })))
}

#[derive(Deserialize)]
pub struct MemberBody {
    role: Option<Role>,
}

pub async fn set_member(State(app): State<AppState>, a: Auth, Path((id, user)): Path<(String, String)>, Json(b): Json<MemberBody>) -> ApiResult<Json<Value>> {
    let p = project_or_404(&app, &a, &id, Role::Admin)?;
    let name: String =
        app.db.row("SELECT name FROM users WHERE id = ?1 OR name = ?1", [&user], |r| r.get(0))?.ok_or_else(|| ApiError::not_found("no such user"))?;
    match b.role {
        Some(role) => app.db.exec(
            "INSERT INTO project_access (user_id, project_id, role) SELECT id, ?2, ?3 FROM users WHERE id = ?1 OR name = ?1
             ON CONFLICT(user_id, project_id) DO UPDATE SET role = excluded.role",
            params![user, p.id, role.as_str()],
        )?,
        None => app.db.exec(
            "DELETE FROM project_access WHERE project_id = ?2 AND user_id IN (SELECT id FROM users WHERE id = ?1 OR name = ?1)",
            params![user, p.id],
        )?,
    };
    app.audit(Some(&a.0), "project.member", Some(&p.id), None, Some(&name), b.role.map(|r| r.as_str()));
    Ok(ok())
}

fn binds(s: &ServiceSpec) -> impl Iterator<Item = &String> {
    s.mounts.iter().filter_map(|m| if let Mount::Bind { source, .. } = m { Some(source) } else { None })
}

fn volumes(s: &ServiceSpec) -> impl Iterator<Item = &String> {
    s.mounts.iter().filter_map(|m| if let Mount::Volume { name, .. } = m { Some(name) } else { None })
}

/// Settings that can hand out host-level access need the destroy grant (or the owner).
fn check_dangerous(a: &Auth, project_id: &str, old: Option<&ServiceSpec>, new: &ServiceSpec, paths: &crate::paths::Paths) -> ApiResult<()> {
    for source in binds(new) {
        let src = std::path::Path::new(source);
        if !src.is_absolute() {
            return Err(ApiError::bad_request(format!("bind source {source} must be an absolute path")));
        }
        let forbidden = [&paths.etc, &paths.data, &paths.run];
        if src.components().any(|c| c == std::path::Component::ParentDir) || forbidden.iter().any(|f| src.starts_with(f)) {
            return Err(ApiError::bad_request("services cannot mount Tugport's own files"));
        }
        if source.ends_with("docker.sock") && !new.docker_socket {
            return Err(ApiError::bad_request("use the docker_socket switch to give a service the Docker socket"));
        }
    }
    let changed = |f: fn(&ServiceSpec) -> bool| f(new) && old.is_none_or(|o| !f(o));
    let new_binds = binds(new).any(|b| old.is_none_or(|o| !binds(o).any(|x| x == b)));
    let new_caps = new.cap_add.iter().any(|c| old.is_none_or(|o| !o.cap_add.contains(c)));
    // Joining a network by name reaches whatever is on it, such as another project's databases.
    let new_networks = new.networks.iter().any(|n| old.is_none_or(|o| !o.networks.contains(n)));
    if (changed(|s| s.privileged) || changed(|s| s.docker_socket) || changed(|s| s.host_network) || new_binds || new_caps || new_networks)
        && !a.0.can_destroy(project_id)
    {
        return Err(ApiError::forbidden(
            "privileged mode, the Docker socket, host networking, bind mounts, extra capabilities and extra networks need the admin role and the destroy grant",
        ));
    }
    let in_repo = |p: &str| util::inside(std::path::Path::new("/"), p).is_some();
    // Deploys check these again; refusing them here gives the error before the first deploy.
    if let Source::Git { provider, repo, branch, path, .. } = &new.source {
        if !crate::git::valid_repo(repo, provider.is_some()) {
            return Err(ApiError::bad_request(format!("{repo} is not a repository Tugport can fetch: use an https://, ssh:// or git@host: address")));
        }
        let repo_changed = old.is_none_or(|o| !matches!(&o.source, Source::Git { repo: r, .. } if r == repo));
        if repo_changed && crate::git::url_has_credentials(repo) {
            return Err(ApiError::bad_request(
                "the repository address holds credentials, which everyone who can see the service would read; connect a Git provider or add a deploy key instead",
            ));
        }
        if !crate::git::valid_ref(branch) {
            return Err(ApiError::bad_request(format!("{branch} is not a branch name")));
        }
        if !in_repo(path) {
            return Err(ApiError::bad_request("the path must be a folder inside the repository"));
        }
    }
    if new.compose.as_ref().is_some_and(|c| !in_repo(&c.file)) || matches!(&new.build, Build::Dockerfile { path, .. } if !in_repo(path)) {
        return Err(ApiError::bad_request("file paths must stay inside the repository"));
    }
    if new.replicas > 100 {
        return Err(ApiError::bad_request("at most 100 replicas"));
    }
    Ok(())
}

/// Docker volumes are shared by every service on a server, so a volume a spec adds must be new
/// on every server or already belong to this project (the admin role with the destroy grant may
/// attach any volume, such as one a migration left).
async fn check_volumes(app: &AppState, a: &Auth, project_id: &str, old: Option<&ServiceSpec>, new: &ServiceSpec) -> ApiResult<()> {
    if a.0.can_destroy(project_id) {
        return Ok(());
    }
    let added: Vec<&String> = volumes(new).filter(|v| old.is_none_or(|o| !volumes(o).any(|x| x == *v))).collect();
    if added.is_empty() {
        return Ok(());
    }
    let services = store::services(app, None)?;
    for name in added {
        let users: Vec<&Service> = services.iter().filter(|s| volumes(&s.spec).any(|v| v == name)).collect();
        if name.starts_with("tugport") || users.iter().any(|s| s.project_id != project_id) {
            return Err(ApiError::forbidden(format!("the volume {name} belongs to another project or to Tugport; pick another name")));
        }
        if users.is_empty() {
            for (_, docker) in crate::runtime::dockers(app) {
                if docker.inspect_volume(name).await.is_ok() {
                    return Err(ApiError::forbidden(format!(
                        "a volume named {name} already exists on a server; pick another name, or ask an admin with the destroy grant to attach it"
                    )));
                }
            }
        }
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
    // Container, image, network-alias and Compose project names start with this, and Compose
    // merges stacks of the same name: `shop` + `api-v2` and `shop-api` + `v2` would collide.
    let slug = format!("tp-{}-{name}", p.name);
    if let Some(other) = store::services(&app, None)?.into_iter().find(|o| o.slug() == slug) {
        return Err(ApiError::conflict(format!("{slug} is already the name of {}/{}; pick another service name", other.project, other.name)));
    }
    let mut spec: ServiceSpec = match b.spec {
        Some(v) => serde_json::from_value(v).map_err(|e| ApiError::bad_request(format!("invalid spec: {e}")))?,
        None => ServiceSpec::default(),
    };
    let mut env = b.env;
    // The new database's connection URL and the project variable that will hold it.
    let mut database = None;
    match b.kind.as_str() {
        "app" => {
            if matches!(spec.source, Source::Compose { .. }) {
                return Err(ApiError::bad_request("use kind \"compose\" for Compose stacks"));
            }
            if matches!(&spec.source, Source::Image { image } if image.trim().is_empty()) {
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
            database = Some(prepare_database(&p, &name, db, &mut spec, &mut env).await?);
        }
        other => return Err(ApiError::bad_request(format!("unknown kind {other}; use app, database or compose"))),
    }
    check_dangerous(&a, &p.id, None, &spec, &app.paths)?;
    check_volumes(&app, &a, &p.id, None, &spec).await?;
    check_env_keys(env.keys())?;
    let id = util::id();
    app.db.exec(
        "INSERT INTO services (id, project_id, name, kind, spec, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, p.id, name, b.kind, serde_json::to_string(&spec)?, util::now()],
    )?;
    for (k, v) in &env {
        store::set_env(&app, &id, k, Some(v))?;
    }
    // Only once every check passed, so a refused database leaves no variable behind.
    if let Some((url, var)) = &database
        && !store::project_vars(&app, &p.id)?.contains_key(var)
    {
        store::set_project_var(&app, &p.id, var, Some(url))?;
    }
    app.audit(Some(&a.0), "service.create", Some(&p.id), Some(&id), Some(&name), Some(&b.kind));
    // A database has no source to deploy from, so no deploy webhook either.
    let hook = if b.kind == "database" { None } else { Some(new_hook_url(&app, &id, &headers)?) };
    super::git::ensure_hook_soon(&app, &headers, &id)?;
    let database = database.map(|(url, var)| json!({ "url": url, "project_var": var }));
    Ok(Json(json!({ "id": id, "name": name, "kind": b.kind, "spec": spec, "deploy_hook": hook, "database": database })))
}

/// Fill in a database service's spec and env; returns its connection URL and the name of the
/// project variable for it.
async fn prepare_database(p: &store::Project, name: &str, mut db: DatabaseSpec, spec: &mut ServiceSpec, env: &mut BTreeMap<String, String>) -> ApiResult<(String, String)> {
    let preset = databases::preset(db.engine);
    let image = match &spec.source {
        Source::Image { image } if !image.trim().is_empty() => image.clone(),
        _ => format!("{}:{}", preset.repo, databases::latest_version(db.engine).await),
    };
    let version = image.rsplit_once(':').map(|(_, v)| v).unwrap_or_default();
    let data_dir = databases::data_dir(db.engine, version);
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
    // These become the database's user and schema and appear in the connection URL and in the
    // replica setup scripts; keep them to plain identifiers so neither is malformed.
    let ident = |s: &str| !s.is_empty() && s.len() <= 63 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') && !s.starts_with(['-', '_']);
    if !ident(&db.user) || !ident(&db.database) {
        return Err(ApiError::bad_request("the database user and name are letters, digits, underscores and dashes"));
    }
    let password = util::token(32);
    for (k, v) in databases::init_env(db.engine, &db.user, &db.database, &password) {
        env.entry(k.into()).or_insert(v);
    }
    spec.mounts.retain(|m| m.target() != data_dir);
    spec.mounts.push(Mount::Volume { name: format!("tp-{}-{name}-data", p.name), target: data_dir.into(), readonly: false });
    spec.source = Source::Image { image };
    spec.build = Build::Auto;
    spec.command = databases::command(db.engine);
    spec.health = Health::database(databases::preset(db.engine).health);
    // Two copies of a database must never share a data directory.
    spec.rollout.strategy = Strategy::StopFirst;
    spec.replicas = 1;
    let url = databases::url(db.engine, name, &db.user, &password, &db.database);
    spec.database = Some(db);
    Ok((url, format!("{}_URL", name.to_uppercase().replace('-', "_"))))
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
        "exposed_ports": exposed, "role": role(&a, &s.project_id),
    })))
}

#[derive(Deserialize)]
pub struct UpdateService {
    spec: Option<Value>,
}

pub async fn update_service(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>, Json(b): Json<UpdateService>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    let Some(patch) = b.spec else { return Ok(Json(json!({ "spec": s.spec }))) };
    let next: ServiceSpec = super::patched(&s.spec, &patch, "spec")?;
    check_dangerous(&a, &s.project_id, Some(&s.spec), &next, &app.paths)?;
    check_volumes(&app, &a, &s.project_id, Some(&s.spec), &next).await?;
    if s.kind == "database" && next.replicas > 1 {
        return Err(ApiError::bad_request("databases run one primary; add read replicas instead"));
    }
    store::save_spec(&app, &s.id, &next)?;
    a.audit_service(&app, "service.update", &s, Some(&patch.as_object().map(|o| names(o.keys())).unwrap_or_default()));
    app.emit("service.updated", Some(&s.project_id), Some(&s.id), json!({}));
    super::git::ensure_hook_soon(&app, &headers, &s.id)?;
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
    require_destroy(&a, &s.project_id)?;
    if !crate::replicas::of(&app, &s.id)?.is_empty() {
        return Err(ApiError::conflict("this database has replicas; promote or delete them first"));
    }
    crate::deploy::cancel_all(&app, &s.id);
    crate::runtime::remove_service(&app, &s, q.volumes).await?;
    app.db.exec("DELETE FROM services WHERE id = ?1", [&s.id])?;
    a.audit_service(&app, "service.delete", &s, q.volumes.then_some("with volumes"));
    crate::proxy::reload(&app);
    Ok(ok())
}

pub async fn get_env(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let can = a.0.can_read_secrets(&s.project_id);
    let project: Vec<String> = store::project_vars(&app, &s.project_id)?.into_keys().collect();
    Ok(Json(json!({ "env": reveal(store::service_env(&app, &s.id)?, can), "project_vars": project, "values_visible": can })))
}

pub async fn put_env(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<BTreeMap<String, Option<String>>>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    check_env_keys(b.keys())?;
    for (k, v) in &b {
        store::set_env(&app, &s.id, k, v.as_deref())?;
    }
    a.audit_service(&app, "service.env", &s, Some(&names(b.keys())));
    Ok(redeploy_needed())
}

pub async fn list_files(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let can = a.0.can_read_secrets(&s.project_id);
    let files: Vec<Value> = store::files(&app, &s.id)?
        .into_iter()
        .map(|(t, c)| if can { json!({ "target": t, "content": c.as_str() }) } else { json!({ "target": t }) })
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
    Ok(redeploy_needed())
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
async fn hash_basic_auth(mut opts: DomainOptions, previous: Option<&DomainOptions>) -> ApiResult<DomainOptions> {
    for u in &mut opts.basic_auth {
        match u.password.take().filter(|p| !p.is_empty()) {
            Some(p) => u.hash = super::hash_password(p).await?,
            None if !u.hash.is_empty() => {}
            None => {
                let kept = previous.and_then(|p| p.basic_auth.iter().find(|o| o.user == u.user)).map(|o| o.hash.clone());
                u.hash = kept.ok_or_else(|| ApiError::bad_request(format!("set a password for {}", u.user)))?;
            }
        }
    }
    Ok(opts)
}

/// Routing a name through the tunnel creates or changes its DNS record in the owner's
/// Cloudflare account, so that is the owner's call.
fn check_tunnel(a: &Auth, before: bool, after: bool) -> ApiResult<()> {
    if after && !before && !a.0.is_owner {
        return Err(ApiError::forbidden("routing a domain through the Cloudflare Tunnel changes DNS records in the owner's Cloudflare account, so only the owner can turn it on"));
    }
    Ok(())
}

fn check_domain(app: &AppState, project_id: &str, host: &str, path: &str, port: u16, opts: &DomainOptions) -> ApiResult<()> {
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
    if let Some(fa) = &opts.forward_auth
        && !(fa.url.starts_with("https://") || fa.url.starts_with("http://"))
    {
        return Err(ApiError::bad_request("the forward auth address is an http:// or https:// URL"));
    }
    // A host belongs to one project, and the panel's own domain to Tugport: otherwise a route on
    // it (a longer path, say) would take another project's or the panel's requests and cookies.
    if app.settings().panel_domain.is_some_and(|d| d.eq_ignore_ascii_case(host)) {
        return Err(ApiError::forbidden(format!("{host} is the panel's own domain")));
    }
    let taken: bool = app.db.conn().query_row(
        "SELECT COUNT(*) > 0 FROM domains d JOIN services s ON s.id = d.service_id WHERE d.host = ?1 AND s.project_id != ?2",
        params![host, project_id],
        |r| r.get(0),
    )?;
    if taken {
        return Err(ApiError::conflict(format!("{host} is already used by another project")));
    }
    Ok(())
}

/// A domain and its service, which the caller must operate.
fn domain_or_404(app: &AppState, a: &Auth, id: &str) -> ApiResult<(Domain, Service)> {
    let d = store::domain(app, id)?.ok_or_else(|| ApiError::not_found("domain not found"))?;
    let s = service_or_404(app, a, &d.service_id, Role::Operator)?;
    Ok((d, s))
}

fn domains_changed(app: &AppState, a: &Auth, action: &str, s: &Service, host: &str) {
    app.audit(Some(&a.0), action, Some(&s.project_id), Some(&s.id), Some(host), None);
    crate::proxy::reload(app);
    crate::tunnel::sync_soon();
}

pub async fn create_domain(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<DomainBody>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    let host = b.host.unwrap_or_default().trim().to_lowercase();
    let path = b.path.unwrap_or_else(|| "/".into());
    let port = b.port.or_else(|| s.spec.database.as_ref().map(|d| databases::preset(d.engine).port)).unwrap_or(0);
    let via_tunnel = b.via_tunnel.unwrap_or(false);
    let opts = hash_basic_auth(b.options.unwrap_or_default(), None).await?;
    check_domain(&app, &s.project_id, &host, &path, port, &opts)?;
    check_tunnel(&a, false, via_tunnel)?;
    let did = util::id();
    app.db.exec(
        "INSERT INTO domains (id, service_id, host, path, port, container, https, via_tunnel, options, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![did, s.id, host, path, port, b.container.flatten(), b.https.unwrap_or(true), via_tunnel, serde_json::to_string(&opts)?, util::now()],
    )?;
    domains_changed(&app, &a, "domain.create", &s, &host);
    Ok(Json(json!({ "domain": store::domain(&app, &did)? })))
}

pub async fn update_domain(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<DomainBody>) -> ApiResult<Json<Value>> {
    let (d, s) = domain_or_404(&app, &a, &id)?;
    let host = b.host.map(|h| h.trim().to_lowercase()).unwrap_or(d.host);
    let path = b.path.unwrap_or(d.path);
    let port = b.port.unwrap_or(d.port);
    let via_tunnel = b.via_tunnel.unwrap_or(d.via_tunnel);
    let opts = match b.options {
        Some(o) => hash_basic_auth(o, Some(&d.options)).await?,
        None => d.options,
    };
    check_domain(&app, &s.project_id, &host, &path, port, &opts)?;
    check_tunnel(&a, d.via_tunnel, via_tunnel)?;
    app.db.exec(
        "UPDATE domains SET host = ?1, path = ?2, port = ?3, container = ?4, https = ?5, via_tunnel = ?6, options = ?7 WHERE id = ?8",
        params![host, path, port, b.container.unwrap_or(d.container), b.https.unwrap_or(d.https), via_tunnel, serde_json::to_string(&opts)?, id],
    )?;
    domains_changed(&app, &a, "domain.update", &s, &host);
    Ok(Json(json!({ "domain": store::domain(&app, &id)? })))
}

pub async fn delete_domain(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (d, s) = domain_or_404(&app, &a, &id)?;
    app.db.exec("DELETE FROM domains WHERE id = ?1", [&id])?;
    domains_changed(&app, &a, "domain.delete", &s, &d.host);
    Ok(ok())
}

fn hook_url(headers: &HeaderMap, token: &str) -> String {
    format!("{}/api/hooks/deploy/{token}", base_url(headers))
}

fn new_hook_url(app: &AppState, service_id: &str, headers: &HeaderMap) -> ApiResult<String> {
    let token = util::token(32);
    app.db.exec("UPDATE services SET deploy_token_hash = ?1 WHERE id = ?2", params![util::sha256_hex(&token), service_id])?;
    // Kept sealed too, so people allowed to read secrets can see the URL again.
    let key = format!("hook:{service_id}");
    app.set_sealed_state(&key, &key, &token)?;
    Ok(hook_url(headers, &token))
}

/// The current deploy webhook URL (it deploys the service, so it needs secrets:read).
pub async fn get_hook(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if !a.0.can_read_secrets(&s.project_id) {
        return Err(ApiError::forbidden("the webhook URL is a secret; it needs the secrets:read grant"));
    }
    let key = format!("hook:{}", s.id);
    let token = app.sealed_state(&key, &key)?.ok_or_else(|| ApiError::not_found("no webhook URL is kept for this service; rotate it to make one"))?;
    Ok(Json(json!({ "url": hook_url(&headers, &token) })))
}

/// Replace the service's deploy webhook URL (the old one stops working).
pub async fn rotate_hook(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if s.kind == "database" {
        return Err(ApiError::bad_request("databases have no deploy webhook; redeploy or restart them from the panel"));
    }
    let url = new_hook_url(&app, &s.id, &headers)?;
    a.audit_service(&app, "service.hook_rotated", &s, None);
    Ok(Json(json!({ "url": url })))
}
