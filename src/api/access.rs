//! First-run setup, sign-in, users, agents, API keys, instance settings and the audit log.

use super::{ApiError, ApiResult, Auth, ClientIp, base_url};
use crate::app::{AppState, Settings};
use crate::auth::{self, GRANTS, Role, UserKind};
use crate::util;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub async fn setup_status(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    let users: i64 = app.db.conn().query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    Ok(Json(json!({ "needs_setup": users == 0 })))
}

#[derive(Deserialize)]
pub struct SetupBody {
    token: String,
    name: String,
    email: Option<String>,
    password: String,
}

pub async fn setup(State(app): State<AppState>, ClientIp(ip): ClientIp, headers: HeaderMap, Json(b): Json<SetupBody>) -> ApiResult<impl IntoResponse> {
    let users: i64 = app.db.conn().query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if users > 0 {
        return Err(ApiError::conflict("setup is already done"));
    }
    let expected = app.db.get_state("setup_token")?.ok_or_else(|| ApiError::forbidden("no setup token; run `tugport setup-link` on the server"))?;
    if !util::ct_eq(&util::sha256_hex(b.token.trim()), &expected) {
        return Err(ApiError::forbidden("the setup link is not valid"));
    }
    validate_password(&b.password)?;
    let name = b.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("pick a user name"));
    }
    let id = util::id();
    app.db.conn().execute(
        "INSERT INTO users (id, kind, name, email, password_hash, is_owner, created_at) VALUES (?1, 'human', ?2, ?3, ?4, 1, ?5)",
        params![id, name, b.email, auth::hash_password(&b.password)?, util::now()],
    )?;
    app.db.conn().execute("DELETE FROM state WHERE key = 'setup_token'", [])?;
    let p = auth::load_principal(&app.db, &id)?;
    app.audit(p.as_ref(), "setup.owner_created", None, None, Some(name), None);
    let token = auth::create_session(&app.db, &id, ip.map(|i| i.to_string()).as_deref(), user_agent(&headers))?;
    Ok((AppendHeaders([(header::SET_COOKIE, session_cookie(&headers, &token, 30 * 86400))]), Json(json!({ "ok": true }))))
}

fn validate_password(p: &str) -> ApiResult<()> {
    if p.chars().count() < 10 {
        return Err(ApiError::bad_request("use a password of at least 10 characters"));
    }
    Ok(())
}

fn user_agent(h: &HeaderMap) -> Option<&str> {
    h.get(header::USER_AGENT).and_then(|v| v.to_str().ok())
}

fn session_cookie(headers: &HeaderMap, token: &str, max_age: i64) -> String {
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
        auth::SESSION_COOKIE,
        if secure { "; Secure" } else { "" }
    )
}

#[derive(Deserialize)]
pub struct LoginBody {
    name: String,
    password: String,
    totp: Option<String>,
}

/// Failed sign-ins per address and failed 2FA codes per user, within the last 15 minutes.
static FAILURES: std::sync::Mutex<BTreeMap<String, Vec<i64>>> = std::sync::Mutex::new(BTreeMap::new());
const FAILURE_WINDOW: i64 = 900;

/// Refuses for a while once `key` failed `limit` times within the window.
fn throttle(key: &str, limit: usize) -> ApiResult<()> {
    let now = util::now();
    let mut all = util::lock(&FAILURES);
    let times = all.entry(key.to_string()).or_default();
    times.retain(|t| now - t < FAILURE_WINDOW);
    match times.first() {
        Some(first) if times.len() >= limit => {
            let minutes = (FAILURE_WINDOW - (now - first)) / 60 + 1;
            Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, format!("too many failed attempts; try again in {minutes} minutes")))
        }
        _ => Ok(()),
    }
}

fn failed(key: &str) {
    let now = util::now();
    let mut all = util::lock(&FAILURES);
    all.entry(key.to_string()).or_default().push(now);
    if all.len() > 10_000 {
        all.retain(|_, t| t.last().is_some_and(|l| now - l < FAILURE_WINDOW));
    }
}

/// The current code (or one step either side), and each code only once.
fn totp_ok(totp: &totp_rs::TOTP, code: &str, user_id: &str) -> bool {
    static USED: std::sync::Mutex<BTreeMap<String, u64>> = std::sync::Mutex::new(BTreeMap::new());
    let now = util::now().max(0) as u64;
    let mut used = util::lock(&USED);
    let last = used.get(user_id).copied().unwrap_or(0);
    for t in [now.saturating_sub(30), now, now + 30] {
        if t / 30 > last && util::ct_eq(&totp.generate(t), code) {
            used.insert(user_id.to_string(), t / 30);
            return true;
        }
    }
    false
}

pub async fn login(State(app): State<AppState>, ClientIp(ip): ClientIp, headers: HeaderMap, Json(b): Json<LoginBody>) -> ApiResult<impl IntoResponse> {
    let from = format!("login:{}", ip.map(|i| i.to_string()).unwrap_or_default());
    throttle(&from, 20)?;
    let row: Option<(String, Option<String>, Option<Vec<u8>>)> = app
        .db
        .conn()
        .query_row(
            "SELECT id, password_hash, totp_secret FROM users WHERE kind = 'human' AND disabled = 0 AND (name = ?1 OR email = ?1) ORDER BY name = ?1 DESC LIMIT 1",
            [b.name.trim()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let verified = match row {
        Some((id, Some(hash), totp_secret)) => {
            let password = b.password.clone();
            auth::hashing(move || auth::verify_password(&password, &hash)).await.unwrap_or(false).then_some((id, totp_secret))
        }
        _ => None,
    };
    let Some((id, totp_secret)) = verified else {
        failed(&from);
        app.audit(None, "auth.login_failed", None, None, Some(b.name.trim()), ip.map(|i| i.to_string()).as_deref());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return Err(ApiError::unauthorized("wrong name or password"));
    };
    if let Some(sealed) = totp_secret {
        let secret = app.keys.open(&format!("totp:{id}"), &sealed)?;
        let code = b.totp.as_deref().unwrap_or("").trim().to_string();
        if code.is_empty() {
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "totp_required"));
        }
        // Only someone who knows the password gets this far, so this limit cannot lock the owner out.
        let codes = format!("totp:{id}");
        throttle(&codes, 5)?;
        if !totp_ok(&auth::totp(&secret, &b.name)?, &code, &id) {
            failed(&codes);
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return Err(ApiError::unauthorized("the 2FA code is wrong"));
        }
    }
    let token = auth::create_session(&app.db, &id, ip.map(|i| i.to_string()).as_deref(), user_agent(&headers))?;
    let p = auth::load_principal(&app.db, &id)?;
    app.audit(p.as_ref(), "auth.login", None, None, None, None);
    Ok((AppendHeaders([(header::SET_COOKIE, session_cookie(&headers, &token, 30 * 86400))]), Json(json!({ "ok": true }))))
}

pub async fn logout(State(app): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    if let Some(t) = super::cookie(&headers, auth::SESSION_COOKIE) {
        auth::delete_session(&app.db, &t)?;
    }
    Ok((AppendHeaders([(header::SET_COOKIE, session_cookie(&headers, "", 0))]), Json(json!({ "ok": true }))))
}

pub async fn me(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let (email, has_totp): (Option<String>, bool) = app.db.conn().query_row(
        "SELECT email, totp_secret IS NOT NULL FROM users WHERE id = ?1",
        [&a.0.user_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Json(json!({ "principal": a.0, "email": email, "totp": has_totp })))
}

pub async fn totp_begin(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    if a.0.kind != UserKind::Human {
        return Err(ApiError::bad_request("agents sign in with keys only"));
    }
    // Setting it up again would replace the secret without a current code, unlike turning it off.
    let enabled: bool = app.db.conn().query_row("SELECT totp_secret IS NOT NULL FROM users WHERE id = ?1", [&a.0.user_id], |r| r.get(0))?;
    if enabled {
        return Err(ApiError::bad_request("2FA is already on; turn it off with a current code first"));
    }
    let secret = auth::new_totp_secret();
    let totp = auth::totp(&secret, &a.0.name)?;
    let sealed = app.keys.seal(&format!("totp-pending:{}", a.0.user_id), &secret)?;
    app.db.set_state(&format!("totp_pending:{}", a.0.user_id), &crate::crypto::b64(&sealed))?;
    Ok(Json(json!({ "secret": totp.get_secret_base32(), "url": totp.get_url() })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    code: String,
}

pub async fn totp_confirm(State(app): State<AppState>, a: Auth, Json(b): Json<CodeBody>) -> ApiResult<Json<Value>> {
    let key = format!("totp_pending:{}", a.0.user_id);
    let pending = app.db.get_state(&key)?.ok_or_else(|| ApiError::bad_request("start 2FA setup first"))?;
    let secret = app.keys.open(&format!("totp-pending:{}", a.0.user_id), &crate::crypto::unb64(&pending)?)?;
    if !auth::totp(&secret, &a.0.name)?.check_current(b.code.trim()).unwrap_or(false) {
        return Err(ApiError::bad_request("that code does not match; check the time on your phone"));
    }
    let sealed = app.keys.seal(&format!("totp:{}", a.0.user_id), &secret)?;
    app.db.conn().execute("UPDATE users SET totp_secret = ?1 WHERE id = ?2", params![sealed, a.0.user_id])?;
    app.db.conn().execute("DELETE FROM state WHERE key = ?1", [&key])?;
    app.audit(Some(&a.0), "auth.totp_enabled", None, None, None, None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn totp_disable(State(app): State<AppState>, a: Auth, Json(b): Json<CodeBody>) -> ApiResult<Json<Value>> {
    let sealed: Option<Vec<u8>> =
        app.db.conn().query_row("SELECT totp_secret FROM users WHERE id = ?1", [&a.0.user_id], |r| r.get(0))?;
    let Some(sealed) = sealed else { return Ok(Json(json!({ "ok": true }))) };
    let secret = app.keys.open(&format!("totp:{}", a.0.user_id), &sealed)?;
    let codes = format!("totp:{}", a.0.user_id);
    throttle(&codes, 5)?;
    if !totp_ok(&auth::totp(&secret, &a.0.name)?, b.code.trim(), &a.0.user_id) {
        failed(&codes);
        return Err(ApiError::bad_request("that code does not match"));
    }
    app.db.conn().execute("UPDATE users SET totp_secret = NULL WHERE id = ?1", [&a.0.user_id])?;
    app.audit(Some(&a.0), "auth.totp_disabled", None, None, None, None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn about(State(app): State<AppState>, _a: Auth) -> ApiResult<Json<Value>> {
    let docker = app.docker.version().await.ok();
    Ok(Json(json!({
        "version": crate::VERSION,
        "commit": crate::COMMIT,
        "source": "https://github.com/backmeupplz/tugport",
        "docker": docker.and_then(|v| v.version),
    })))
}

#[derive(Serialize)]
struct UserOut {
    id: String,
    kind: String,
    name: String,
    email: Option<String>,
    description: Option<String>,
    is_owner: bool,
    all_projects_role: Option<String>,
    projects: BTreeMap<String, String>,
    grants: Vec<String>,
    disabled: bool,
    totp: bool,
    created_at: i64,
}

fn read_user(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<UserOut>> {
    let user = conn
        .query_row(
            "SELECT id, kind, name, email, description, is_owner, all_projects_role, grants, disabled, totp_secret IS NOT NULL, created_at FROM users WHERE id = ?1",
            [id],
            |r| {
                Ok(UserOut {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    email: r.get(3)?,
                    description: r.get(4)?,
                    is_owner: r.get(5)?,
                    all_projects_role: r.get(6)?,
                    projects: BTreeMap::new(),
                    grants: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                    disabled: r.get(8)?,
                    totp: r.get(9)?,
                    created_at: r.get(10)?,
                })
            },
        )
        .optional()?;
    let Some(mut user) = user else { return Ok(None) };
    let mut stmt = conn.prepare("SELECT project_id, role FROM project_access WHERE user_id = ?1")?;
    user.projects = stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
    Ok(Some(user))
}

pub async fn list_users(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let conn = app.db.conn();
    let ids: Vec<String> =
        conn.prepare("SELECT id FROM users ORDER BY created_at")?.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
    let mut out = vec![];
    for id in ids {
        if let Some(u) = read_user(&conn, &id)? {
            if a.0.is_owner || u.id == a.0.user_id {
                out.push(serde_json::to_value(u)?);
            } else {
                out.push(json!({ "id": u.id, "kind": u.kind, "name": u.name }));
            }
        }
    }
    Ok(Json(json!({ "users": out })))
}

pub async fn get_user(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.user_id != id {
        return Err(ApiError::forbidden("only the owner can see other users"));
    }
    let u = read_user(&app.db.conn(), &id)?.ok_or_else(|| ApiError::not_found("no such user"))?;
    Ok(Json(serde_json::to_value(u)?))
}

/// Access settings shared by user creation, agent creation and updates.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct AccessSpec {
    /// Role on every project, including future ones.
    all_projects_role: Option<Role>,
    /// Role per project id.
    projects: BTreeMap<String, Role>,
    grants: Vec<String>,
}

fn apply_access(conn: &rusqlite::Connection, user_id: &str, a: &AccessSpec) -> ApiResult<()> {
    apply_access_parts(conn, user_id, Some(a.all_projects_role), Some(&a.projects), Some(&a.grants))
}

/// Change only the parts of a user's access that were given.
fn apply_access_parts(
    conn: &rusqlite::Connection,
    user_id: &str,
    all_projects_role: Option<Option<Role>>,
    projects: Option<&BTreeMap<String, Role>>,
    grants: Option<&[String]>,
) -> ApiResult<()> {
    if let Some(grants) = grants {
        if let Some(g) = grants.iter().find(|g| !GRANTS.contains(&g.as_str())) {
            return Err(ApiError::bad_request(format!("unknown grant {g}; known: {}", GRANTS.join(", "))));
        }
        conn.execute("UPDATE users SET grants = ?1 WHERE id = ?2", params![serde_json::to_string(grants)?, user_id])?;
    }
    if let Some(role) = all_projects_role {
        conn.execute("UPDATE users SET all_projects_role = ?1 WHERE id = ?2", params![role.map(|r| r.as_str()), user_id])?;
    }
    let Some(projects) = projects else { return Ok(()) };
    conn.execute("DELETE FROM project_access WHERE user_id = ?1", [user_id])?;
    for (project, role) in projects {
        let exists: bool = conn.query_row("SELECT COUNT(*) > 0 FROM projects WHERE id = ?1 OR name = ?1", [project], |r| r.get(0))?;
        if !exists {
            return Err(ApiError::bad_request(format!("no project {project}")));
        }
        conn.execute(
            "INSERT INTO project_access (user_id, project_id, role) SELECT ?1, id, ?3 FROM projects WHERE id = ?2 OR name = ?2",
            params![user_id, project, role.as_str()],
        )?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateUserBody {
    kind: UserKind,
    name: String,
    email: Option<String>,
    password: Option<String>,
    description: Option<String>,
    #[serde(flatten)]
    access: AccessSpec,
}

pub async fn create_user(State(app): State<AppState>, a: Auth, Json(b): Json<CreateUserBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let id = new_user(&app, &a, b.kind, &b.name, b.email.as_deref(), b.password.as_deref(), b.description.as_deref(), &b.access)?;
    let u = read_user(&app.db.conn(), &id)?;
    Ok(Json(serde_json::to_value(u)?))
}

#[allow(clippy::too_many_arguments)]
fn new_user(
    app: &AppState,
    a: &Auth,
    kind: UserKind,
    name: &str,
    email: Option<&str>,
    password: Option<&str>,
    description: Option<&str>,
    access: &AccessSpec,
) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("names are 1 to 64 characters"));
    }
    let hash = match (kind, password) {
        (UserKind::Human, Some(p)) => {
            validate_password(p)?;
            Some(auth::hash_password(p)?)
        }
        (UserKind::Human, None) => return Err(ApiError::bad_request("people need a password")),
        (UserKind::Agent, _) => None,
    };
    let id = util::id();
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO users (id, kind, name, email, password_hash, description, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, if kind == UserKind::Agent { "agent" } else { "human" }, name, email, hash, description, a.0.user_id, util::now()],
    )?;
    apply_access(&tx, &id, access)?;
    tx.commit()?;
    drop(conn);
    app.audit(Some(&a.0), if kind == UserKind::Agent { "agent.create" } else { "user.create" }, None, None, Some(name), None);
    Ok(id)
}

/// Fields left out stay as they are; `all_projects_role: null` removes that role.
#[derive(Deserialize)]
pub struct UpdateUserBody {
    name: Option<String>,
    email: Option<String>,
    password: Option<String>,
    /// Needed to change one's own password.
    current_password: Option<String>,
    description: Option<String>,
    disabled: Option<bool>,
    #[serde(default, deserialize_with = "super::present")]
    all_projects_role: Option<Option<Role>>,
    projects: Option<BTreeMap<String, Role>>,
    grants: Option<Vec<String>>,
}


pub async fn update_user(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<UpdateUserBody>) -> ApiResult<Json<Value>> {
    let own = a.0.user_id == id;
    let changes_access = b.all_projects_role.is_some() || b.projects.is_some() || b.grants.is_some();
    if !a.0.is_owner && !(own && !changes_access && b.disabled.is_none()) {
        return Err(ApiError::forbidden("only the owner can change other users or access"));
    }
    if own && b.disabled == Some(true) {
        return Err(ApiError::bad_request("you cannot disable yourself"));
    }
    if b.name.as_deref().is_some_and(|n| n.trim().is_empty() || n.trim().len() > 64) {
        return Err(ApiError::bad_request("names are 1 to 64 characters"));
    }
    // A stolen session or key must not be enough to take the account over for good.
    if own && b.password.is_some() {
        let hash: Option<String> = app.db.conn().query_row("SELECT password_hash FROM users WHERE id = ?1", [&id], |r| r.get(0))?;
        let current = b.current_password.clone().unwrap_or_default();
        let ok = match hash {
            Some(h) => auth::hashing(move || auth::verify_password(&current, &h)).await.unwrap_or(false),
            None => false,
        };
        if !ok {
            return Err(ApiError::forbidden("send your current password (current_password) to change it"));
        }
    }
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    if let Some(n) = &b.name {
        tx.execute("UPDATE users SET name = ?1 WHERE id = ?2", params![n.trim(), id])?;
    }
    if let Some(e) = &b.email {
        tx.execute("UPDATE users SET email = ?1 WHERE id = ?2", params![e, id])?;
    }
    if let Some(d) = &b.description {
        tx.execute("UPDATE users SET description = ?1 WHERE id = ?2", params![d, id])?;
    }
    if let Some(p) = &b.password {
        validate_password(p)?;
        tx.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2 AND kind = 'human'", params![auth::hash_password(p)?, id])?;
        tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
    }
    if let Some(d) = b.disabled {
        tx.execute("UPDATE users SET disabled = ?1 WHERE id = ?2 AND is_owner = 0", params![d, id])?;
        if d {
            tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
        }
    }
    apply_access_parts(&tx, &id, b.all_projects_role, b.projects.as_ref(), b.grants.as_deref())?;
    tx.commit()?;
    let u = read_user(&conn, &id)?.ok_or_else(|| ApiError::not_found("no such user"))?;
    drop(conn);
    app.audit(Some(&a.0), "user.update", None, None, Some(&u.name), None);
    Ok(Json(serde_json::to_value(u)?))
}

pub async fn delete_user(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    if id == a.0.user_id {
        return Err(ApiError::bad_request("you cannot delete yourself"));
    }
    let name: Option<String> = app.db.conn().query_row("SELECT name FROM users WHERE id = ?1", [&id], |r| r.get(0)).optional()?;
    let n = app.db.conn().execute("DELETE FROM users WHERE id = ?1 AND is_owner = 0", [&id])?;
    if n == 0 {
        return Err(ApiError::not_found("no such user"));
    }
    app.audit(Some(&a.0), "user.delete", None, None, name.as_deref(), None);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Serialize)]
struct KeyOut {
    id: String,
    name: String,
    prefix: String,
    expires_at: Option<i64>,
    allowed_ips: Vec<String>,
    last_used_at: Option<i64>,
    last_used_ip: Option<String>,
    created_at: i64,
    revoked_at: Option<i64>,
}

pub async fn list_keys(State(app): State<AppState>, a: Auth, Path(user): Path<String>) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.user_id != user {
        return Err(ApiError::forbidden("only the owner can see other users' keys"));
    }
    let conn = app.db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, name, prefix, expires_at, allowed_ips, last_used_at, last_used_ip, created_at, revoked_at FROM api_keys WHERE user_id = ?1 ORDER BY created_at DESC",
    )?;
    let keys: Vec<KeyOut> = stmt
        .query_map([&user], |r| {
            Ok(KeyOut {
                id: r.get(0)?,
                name: r.get(1)?,
                prefix: r.get(2)?,
                expires_at: r.get(3)?,
                allowed_ips: r.get::<_, Option<String>>(4)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                last_used_at: r.get(5)?,
                last_used_ip: r.get(6)?,
                created_at: r.get(7)?,
                revoked_at: r.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "keys": keys })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct KeyBody {
    name: Option<String>,
    expires_days: Option<u32>,
    allowed_ips: Vec<ipnet::IpNet>,
}

pub async fn create_key(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(user): Path<String>, Json(b): Json<KeyBody>) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.user_id != user {
        return Err(ApiError::forbidden("only the owner can make keys for other users"));
    }
    let name: String = app
        .db
        .conn()
        .query_row("SELECT name FROM users WHERE id = ?1", [&user], |r| r.get(0))
        .optional()?
        .ok_or_else(|| ApiError::not_found("no such user"))?;
    let mut expires = b.expires_days.map(|d| util::now() + d as i64 * 86400);
    let mut ips = b.allowed_ips.clone();
    // A key that makes a key for its own user cannot shed its expiry or address limits that way.
    if let Some(kid) = a.0.key_id.as_deref().filter(|_| user == a.0.user_id) {
        let (_, _, own_expiry, own_ips) = key_owner(&app, kid)?;
        let own_ips: Vec<ipnet::IpNet> = own_ips.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        if let Some(e) = own_expiry {
            expires = Some(expires.map_or(e, |x| x.min(e)));
        }
        if !own_ips.is_empty() {
            if ips.is_empty() {
                ips = own_ips;
            } else if !ips.iter().all(|n| own_ips.iter().any(|o| o.contains(n))) {
                return Err(ApiError::forbidden("a key can only make keys for addresses it may use itself"));
            }
        }
    }
    let k = issue_key(&app, &a, &user, b.name.as_deref().unwrap_or("default"), expires, &ips, &name)?;
    Ok(Json(json!({ "id": k.id, "token": k.token, "snippets": snippets(&base_url(&headers), &k.token) })))
}

fn issue_key(app: &AppState, a: &Auth, user_id: &str, key_name: &str, expires: Option<i64>, ips: &[ipnet::IpNet], user_name: &str) -> ApiResult<auth::NewKey> {
    let k = auth::create_key(&app.db, user_id, key_name, expires, ips)?;
    app.audit(Some(&a.0), "key.create", None, None, Some(&format!("{user_name}/{key_name}")), None);
    Ok(k)
}

#[derive(Deserialize)]
pub struct AgentBody {
    name: String,
    description: Option<String>,
    #[serde(flatten)]
    access: AccessSpec,
    #[serde(default)]
    key: KeyBody,
}

/// Create an agent user and its first key in one step.
pub async fn create_agent(State(app): State<AppState>, a: Auth, headers: HeaderMap, Json(b): Json<AgentBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    if b.access.all_projects_role.is_none() && b.access.projects.is_empty() {
        return Err(ApiError::bad_request("give the agent at least one project or a role on all projects"));
    }
    let id = new_user(&app, &a, UserKind::Agent, &b.name, None, None, b.description.as_deref(), &b.access)?;
    let expires = b.key.expires_days.map(|d| util::now() + d as i64 * 86400);
    let k = issue_key(&app, &a, &id, b.key.name.as_deref().unwrap_or("default"), expires, &b.key.allowed_ips, b.name.trim())?;
    let user = read_user(&app.db.conn(), &id)?;
    Ok(Json(json!({
        "user": user,
        "key": { "id": k.id, "token": k.token },
        "snippets": snippets(&base_url(&headers), &k.token),
    })))
}

/// Ready-to-paste ways to use a key.
fn snippets(base: &str, token: &str) -> Value {
    json!({
        "env": format!("export TUGPORT_URL={base}\nexport TUGPORT_TOKEN={token}"),
        "cli": format!("tugport login {base} --token {token}"),
        "claude_code": format!("claude mcp add --transport http tugport {base}/mcp --header \"Authorization: Bearer {token}\""),
        "codex": format!("# ~/.codex/config.toml (export TUGPORT_TOKEN first)\n[mcp_servers.tugport]\nurl = \"{base}/mcp\"\nbearer_token_env_var = \"TUGPORT_TOKEN\""),
        "curl": format!("curl -H \"Authorization: Bearer {token}\" {base}/api/projects"),
    })
}

fn key_owner(app: &AppState, key_id: &str) -> ApiResult<(String, String, Option<i64>, Option<String>)> {
    app
        .db
        .conn()
        .query_row("SELECT user_id, name, expires_at, allowed_ips FROM api_keys WHERE id = ?1", [key_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?
        .ok_or_else(|| ApiError::not_found("no such key"))
}

pub async fn revoke_key(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (user, name, ..) = key_owner(&app, &id)?;
    if !a.0.is_owner && a.0.user_id != user {
        return Err(ApiError::forbidden("only the owner can revoke other users' keys"));
    }
    app.db.conn().execute("UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![util::now(), id])?;
    app.audit(Some(&a.0), "key.revoke", None, None, Some(&name), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn rotate_key(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (user, name, expires, ips) = key_owner(&app, &id)?;
    if !a.0.is_owner && a.0.user_id != user {
        return Err(ApiError::forbidden("only the owner can rotate other users' keys"));
    }
    let revoked: bool = app.db.conn().query_row("SELECT revoked_at IS NOT NULL FROM api_keys WHERE id = ?1", [&id], |r| r.get(0))?;
    if revoked {
        return Err(ApiError::bad_request("this key was revoked; make a new one instead"));
    }
    let ips: Vec<ipnet::IpNet> = ips.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let k = auth::create_key(&app.db, &user, &name, expires, &ips)?;
    app.db.conn().execute("UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2", params![util::now(), id])?;
    app.audit(Some(&a.0), "key.rotate", None, None, Some(&name), None);
    Ok(Json(json!({ "id": k.id, "token": k.token, "snippets": snippets(&base_url(&headers), &k.token) })))
}

pub async fn get_settings(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Settings>> {
    a.require_owner()?;
    Ok(Json(app.settings()))
}

pub async fn update_settings(State(app): State<AppState>, a: Auth, Json(patch): Json<Value>) -> ApiResult<Json<Settings>> {
    a.require_owner()?;
    let mut current = serde_json::to_value(app.settings())?;
    merge(&mut current, &patch);
    let next: Settings = serde_json::from_value(current).map_err(|e| ApiError::bad_request(format!("invalid settings: {e}")))?;
    app.db.set_setting("instance", &next)?;
    let keys: Vec<&str> = patch.as_object().map(|o| o.keys().map(|k| k.as_str()).collect()).unwrap_or_default();
    app.audit(Some(&a.0), "settings.update", None, None, None, Some(&keys.join(",")));
    app.emit("settings", None, None, json!({}));
    Ok(Json(next))
}

/// JSON merge patch (RFC 7386).
pub fn merge(target: &mut Value, patch: &Value) {
    if let (Some(t), Some(p)) = (target.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else {
        *target = patch.clone();
    }
}

#[derive(Deserialize)]
pub struct AuditQuery {
    project: Option<String>,
    service: Option<String>,
    actor: Option<String>,
    before: Option<i64>,
    limit: Option<u32>,
}

pub async fn audit(State(app): State<AppState>, a: Auth, Query(q): Query<AuditQuery>) -> ApiResult<Json<Value>> {
    if let Some(p) = &q.project {
        a.require(p, Role::Viewer)?;
    } else if !a.0.is_owner && a.0.all_projects_role.is_none() {
        return Err(ApiError::forbidden("pick a project to see its audit log"));
    }
    let conn = app.db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, at, actor_name, actor_kind, key_id, ip, action, project_id, service_id, target, detail FROM audit
         WHERE (?1 IS NULL OR project_id = ?1) AND (?2 IS NULL OR service_id = ?2) AND (?3 IS NULL OR actor_name = ?3)
           AND (?4 IS NULL OR id < ?4) ORDER BY id DESC LIMIT ?5",
    )?;
    let rows: Vec<Value> = stmt
        .query_map(params![q.project, q.service, q.actor, q.before, q.limit.unwrap_or(100).min(500)], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "at": r.get::<_, i64>(1)?, "actor": r.get::<_, Option<String>>(2)?,
                "actor_kind": r.get::<_, Option<String>>(3)?, "key_id": r.get::<_, Option<String>>(4)?, "ip": r.get::<_, Option<String>>(5)?,
                "action": r.get::<_, String>(6)?, "project_id": r.get::<_, Option<String>>(7)?, "service_id": r.get::<_, Option<String>>(8)?,
                "target": r.get::<_, Option<String>>(9)?, "detail": r.get::<_, Option<String>>(10)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "entries": rows })))
}


pub async fn update_status(State(_app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let latest = crate::updates::latest().await.ok();
    let available = latest.as_ref().is_some_and(|r| crate::updates::newer(&r.version));
    Ok(Json(json!({ "current": crate::VERSION, "latest": latest, "available": available })))
}

/// Install the newest release. Running deploys finish first; traffic keeps flowing.
pub async fn update_apply(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let latest = crate::updates::latest().await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    if !crate::updates::newer(&latest.version) {
        return Err(ApiError::bad_request("Tugport is already up to date"));
    }
    app.audit(Some(&a.0), "tugport.update_requested", None, None, Some(&latest.version), None);
    let app2 = app.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::updates::apply(&app2).await {
            tracing::error!("update failed: {e:#}");
            app2.emit("warning", None, None, json!({ "message": format!("Update failed: {e:#}") }));
        }
    });
    Ok(Json(json!({ "updating_to": latest.version })))
}

#[cfg(test)]
mod tests {
    use super::{UpdateUserBody, merge};
    use serde_json::json;

    #[test]
    fn merge_patch() {
        let mut v = json!({ "a": 1, "b": { "c": 2, "d": 3 } });
        merge(&mut v, &json!({ "b": { "c": 9, "d": null }, "e": true }));
        assert_eq!(v, json!({ "a": 1, "b": { "c": 9 }, "e": true }));
    }

    #[test]
    fn user_updates_leave_unsent_access_alone() {
        let b: UpdateUserBody = serde_json::from_value(json!({ "description": "x" })).unwrap();
        assert!(b.all_projects_role.is_none() && b.projects.is_none() && b.grants.is_none());
        let b: UpdateUserBody = serde_json::from_value(json!({ "all_projects_role": null })).unwrap();
        assert_eq!(b.all_projects_role, Some(None));
        let b: UpdateUserBody = serde_json::from_value(json!({ "all_projects_role": "viewer", "grants": [] })).unwrap();
        assert!(matches!(b.all_projects_role, Some(Some(_))) && b.grants == Some(vec![]));
    }
}
