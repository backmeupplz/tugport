//! First-run setup, sign-in, users, agents, API keys, instance settings and the audit log.

use super::{ApiError, ApiResult, Auth, ClientIp, Owner, base_url, hash_password, names, ok};
use crate::app::{AppState, Settings};
use crate::auth::{self, GRANTS, Role, UserKind};
use crate::db::Db;
use crate::util;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{AppendHeaders, IntoResponse};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn needs_setup(app: &AppState) -> ApiResult<bool> {
    Ok(app.db.conn().query_row("SELECT COUNT(*) = 0 FROM users", [], |r| r.get(0))?)
}

pub async fn setup_status(State(app): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "needs_setup": needs_setup(&app)? })))
}

#[derive(Deserialize)]
pub struct SetupBody {
    token: String,
    name: String,
    email: Option<String>,
    password: String,
}

pub async fn setup(State(app): State<AppState>, ClientIp(ip): ClientIp, headers: HeaderMap, Json(b): Json<SetupBody>) -> ApiResult<impl IntoResponse> {
    if !needs_setup(&app)? {
        return Err(ApiError::conflict("setup is already done"));
    }
    let expected = app.db.get_state("setup_token")?.ok_or_else(|| ApiError::forbidden("no setup token; run `tugport setup-link` on the server"))?;
    if !util::ct_eq(util::sha256_hex(b.token.trim()), &expected) {
        return Err(ApiError::forbidden("the setup link is not valid"));
    }
    let name = b.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("pick a user name"));
    }
    let hash = new_password(b.password).await?;
    let id = util::id();
    // Checked again in the insert: hashing above gives a second setup request time to get here too.
    let created = app.db.exec(
        "INSERT INTO users (id, kind, name, email, password_hash, is_owner, created_at)
         SELECT ?1, 'human', ?2, ?3, ?4, 1, ?5 WHERE NOT EXISTS (SELECT 1 FROM users)",
        params![id, name, b.email, hash, util::now()],
    )?;
    if created == 0 {
        return Err(ApiError::conflict("setup is already done"));
    }
    app.db.del_state("setup_token")?;
    app.audit(auth::load_principal(&app.db, &id)?.as_ref(), "setup.owner_created", None, None, Some(name), None);
    signed_in(&app, &headers, &id, ip)
}

/// A new session for `user_id`, set as the cookie.
fn signed_in(app: &AppState, headers: &HeaderMap, user_id: &str, ip: Option<std::net::IpAddr>) -> ApiResult<impl IntoResponse + use<>> {
    let user_agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let token = auth::create_session(&app.db, user_id, ip.map(|i| i.to_string()).as_deref(), user_agent)?;
    Ok((AppendHeaders([(header::SET_COOKIE, session_cookie(headers, &token, 30 * 86400))]), ok()))
}

/// The hash of a password someone picks, once it is long enough.
async fn new_password(p: String) -> ApiResult<String> {
    if p.chars().count() < 10 {
        return Err(ApiError::bad_request("use a password of at least 10 characters"));
    }
    hash_password(p).await
}

fn check_name(name: &str) -> ApiResult<()> {
    if name.is_empty() || name.len() > 64 { Err(ApiError::bad_request("names are 1 to 64 characters")) } else { Ok(()) }
}

/// Argon2 off the async threads; no hash (no password set) never matches.
async fn password_ok(password: String, hash: Option<String>) -> bool {
    let Some(hash) = hash else { return false };
    auth::hashing(move || auth::verify_password(&password, &hash)).await.unwrap_or(false)
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

/// Whether `code` is the user's current 2FA code (or one step either side), each code only
/// once. Five wrong codes within the window refuse further tries for a while.
fn code_ok(user_id: &str, secret: &[u8], code: &str) -> ApiResult<bool> {
    static USED: std::sync::Mutex<BTreeMap<String, u64>> = std::sync::Mutex::new(BTreeMap::new());
    let key = format!("totp:{user_id}");
    throttle(&key, 5)?;
    let mut used = util::lock(&USED);
    let last = used.get(user_id).copied().unwrap_or(0);
    match auth::totp_step(secret, code) {
        Some(step) if step > last => {
            used.insert(user_id.to_string(), step);
            Ok(true)
        }
        _ => {
            failed(&key);
            Ok(false)
        }
    }
}

/// The user's sealed 2FA secret, if 2FA is on.
fn totp_secret(app: &AppState, user_id: &str) -> ApiResult<Option<Vec<u8>>> {
    Ok(app.db.conn().query_row("SELECT totp_secret FROM users WHERE id = ?1", [user_id], |r| r.get(0))?)
}

pub async fn login(State(app): State<AppState>, ClientIp(ip): ClientIp, headers: HeaderMap, Json(b): Json<LoginBody>) -> ApiResult<impl IntoResponse> {
    let from = format!("login:{}", ip.map(|i| i.to_string()).unwrap_or_default());
    throttle(&from, 20)?;
    let row: Option<(String, Option<String>, Option<Vec<u8>>)> = app.db.row(
        "SELECT id, password_hash, totp_secret FROM users WHERE kind = 'human' AND disabled = 0 AND (name = ?1 OR email = ?1) ORDER BY name = ?1 DESC LIMIT 1",
        [b.name.trim()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let verified = match row {
        Some((id, hash, totp_secret)) => password_ok(b.password, hash).await.then_some((id, totp_secret)),
        None => None,
    };
    let Some((id, totp_secret)) = verified else {
        failed(&from);
        app.audit(None, "auth.login_failed", None, None, Some(b.name.trim()), ip.map(|i| i.to_string()).as_deref());
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return Err(ApiError::unauthorized("wrong name or password"));
    };
    if let Some(sealed) = totp_secret {
        let secret = app.keys.open(&format!("totp:{id}"), &sealed)?;
        let code = b.totp.as_deref().unwrap_or("").trim();
        if code.is_empty() {
            return Err(ApiError::unauthorized("totp_required"));
        }
        // Only someone who knows the password gets this far, so this limit cannot lock the owner out.
        if !code_ok(&id, &secret, code)? {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            return Err(ApiError::unauthorized("the 2FA code is wrong"));
        }
    }
    let resp = signed_in(&app, &headers, &id, ip)?;
    app.audit(auth::load_principal(&app.db, &id)?.as_ref(), "auth.login", None, None, None, None);
    Ok(resp)
}

pub async fn logout(State(app): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    if let Some(t) = super::cookie(&headers, auth::SESSION_COOKIE) {
        auth::delete_session(&app.db, &t)?;
    }
    Ok((AppendHeaders([(header::SET_COOKIE, session_cookie(&headers, "", 0))]), ok()))
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
    if totp_secret(&app, &a.0.user_id)?.is_some() {
        return Err(ApiError::bad_request("2FA is already on; turn it off with a current code first"));
    }
    let secret = auth::new_totp_secret();
    let (b32, url) = auth::totp_link(&secret, &a.0.name);
    let sealed = app.keys.seal(&format!("totp-pending:{}", a.0.user_id), &secret)?;
    app.db.set_state(&format!("totp_pending:{}", a.0.user_id), &crate::crypto::b64(&sealed))?;
    Ok(Json(json!({ "secret": b32, "url": url })))
}

#[derive(Deserialize)]
pub struct CodeBody {
    code: String,
}

pub async fn totp_confirm(State(app): State<AppState>, a: Auth, Json(b): Json<CodeBody>) -> ApiResult<Json<Value>> {
    let key = format!("totp_pending:{}", a.0.user_id);
    let pending = app.db.get_state(&key)?.ok_or_else(|| ApiError::bad_request("start 2FA setup first"))?;
    let secret = app.keys.open(&format!("totp-pending:{}", a.0.user_id), &crate::crypto::unb64(&pending)?)?;
    if auth::totp_step(&secret, b.code.trim()).is_none() {
        return Err(ApiError::bad_request("that code does not match; check the time on your phone"));
    }
    let sealed = app.keys.seal(&format!("totp:{}", a.0.user_id), &secret)?;
    app.db.exec("UPDATE users SET totp_secret = ?1 WHERE id = ?2", params![sealed, a.0.user_id])?;
    app.db.del_state(&key)?;
    a.audit(&app, "auth.totp_enabled", None, None);
    Ok(ok())
}

pub async fn totp_disable(State(app): State<AppState>, a: Auth, Json(b): Json<CodeBody>) -> ApiResult<Json<Value>> {
    let Some(sealed) = totp_secret(&app, &a.0.user_id)? else { return Ok(ok()) };
    let secret = app.keys.open(&format!("totp:{}", a.0.user_id), &sealed)?;
    if !code_ok(&a.0.user_id, &secret, b.code.trim())? {
        return Err(ApiError::bad_request("that code does not match"));
    }
    app.db.exec("UPDATE users SET totp_secret = NULL WHERE id = ?1", [&a.0.user_id])?;
    a.audit(&app, "auth.totp_disabled", None, None);
    Ok(ok())
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

fn read_user(db: &Db, id: &str) -> rusqlite::Result<Option<UserOut>> {
    let user = db.row(
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
    )?;
    let Some(mut user) = user else { return Ok(None) };
    user.projects = db.rows("SELECT project_id, role FROM project_access WHERE user_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect();
    Ok(Some(user))
}

fn user_or_404(app: &AppState, id: &str) -> ApiResult<UserOut> {
    read_user(&app.db, id)?.ok_or_else(|| ApiError::not_found("no such user"))
}

/// The owner may act on anyone; others only on themselves.
fn self_or_owner(a: &Auth, user_id: &str, refusal: &str) -> ApiResult<()> {
    if a.0.is_owner || a.0.user_id == user_id { Ok(()) } else { Err(ApiError::forbidden(refusal)) }
}

pub async fn list_users(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let ids: Vec<String> = app.db.rows("SELECT id FROM users ORDER BY created_at", [], |r| r.get(0))?;
    let mut out = vec![];
    for id in ids {
        let Some(u) = read_user(&app.db, &id)? else { continue };
        out.push(if a.0.is_owner || u.id == a.0.user_id { serde_json::to_value(u)? } else { json!({ "id": u.id, "kind": u.kind, "name": u.name }) });
    }
    Ok(Json(json!({ "users": out })))
}

pub async fn get_user(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    self_or_owner(&a, &id, "only the owner can see other users")?;
    Ok(Json(serde_json::to_value(user_or_404(&app, &id)?)?))
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

/// Change only the parts of a user's access that were given.
fn apply_access(
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
        let added = conn.execute(
            "INSERT INTO project_access (user_id, project_id, role) SELECT ?1, id, ?3 FROM projects WHERE id = ?2 OR name = ?2",
            params![user_id, project, role.as_str()],
        )?;
        if added == 0 {
            return Err(ApiError::bad_request(format!("no project {project}")));
        }
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

pub async fn create_user(State(app): State<AppState>, Owner(a): Owner, Json(b): Json<CreateUserBody>) -> ApiResult<Json<Value>> {
    let id = new_user(&app, &a, b).await?;
    Ok(Json(serde_json::to_value(read_user(&app.db, &id)?)?))
}

async fn new_user(app: &AppState, a: &Auth, b: CreateUserBody) -> ApiResult<String> {
    let name = b.name.trim();
    check_name(name)?;
    let hash = match (b.kind, b.password) {
        (UserKind::Human, Some(p)) => Some(new_password(p).await?),
        (UserKind::Human, None) => return Err(ApiError::bad_request("people need a password")),
        (UserKind::Agent, _) => None,
    };
    let agent = b.kind == UserKind::Agent;
    let id = util::id();
    {
        let mut conn = app.db.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO users (id, kind, name, email, password_hash, description, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, if agent { "agent" } else { "human" }, name, b.email, hash, b.description, a.0.user_id, util::now()],
        )?;
        apply_access(&tx, &id, Some(b.access.all_projects_role), Some(&b.access.projects), Some(&b.access.grants))?;
        tx.commit()?;
    }
    app.audit(Some(&a.0), if agent { "agent.create" } else { "user.create" }, None, None, Some(name), None);
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
    let name = b.name.as_deref().map(str::trim);
    if let Some(n) = name {
        check_name(n)?;
    }
    // A stolen session or key must not be enough to take the account over for good.
    if own && b.password.is_some() {
        let hash: Option<String> = app.db.conn().query_row("SELECT password_hash FROM users WHERE id = ?1", [&id], |r| r.get(0))?;
        if !password_ok(b.current_password.unwrap_or_default(), hash).await {
            return Err(ApiError::forbidden("send your current password (current_password) to change it"));
        }
    }
    let password_hash = match b.password {
        Some(p) => Some(new_password(p).await?),
        None => None,
    };
    {
        let mut conn = app.db.conn();
        let tx = conn.transaction()?;
        for (column, value) in [("name", name), ("email", b.email.as_deref()), ("description", b.description.as_deref())] {
            if let Some(v) = value {
                tx.execute(&format!("UPDATE users SET {column} = ?1 WHERE id = ?2"), params![v, id])?;
            }
        }
        if let Some(h) = &password_hash {
            tx.execute("UPDATE users SET password_hash = ?1 WHERE id = ?2 AND kind = 'human'", params![h, id])?;
            tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
        }
        if let Some(d) = b.disabled {
            tx.execute("UPDATE users SET disabled = ?1 WHERE id = ?2 AND is_owner = 0", params![d, id])?;
            if d {
                tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&id])?;
            }
        }
        apply_access(&tx, &id, b.all_projects_role, b.projects.as_ref(), b.grants.as_deref())?;
        tx.commit()?;
    }
    let u = user_or_404(&app, &id)?;
    a.audit(&app, "user.update", Some(&u.name), None);
    Ok(Json(serde_json::to_value(u)?))
}

pub async fn delete_user(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if id == a.0.user_id {
        return Err(ApiError::bad_request("you cannot delete yourself"));
    }
    let name: Option<String> = app.db.row("SELECT name FROM users WHERE id = ?1", [&id], |r| r.get(0))?;
    if app.db.exec("DELETE FROM users WHERE id = ?1 AND is_owner = 0", [&id])? == 0 {
        return Err(ApiError::not_found("no such user"));
    }
    a.audit(&app, "user.delete", name.as_deref(), None);
    Ok(ok())
}

/// A key's `allowed_ips` column (a JSON list, or NULL for none).
fn parse_ips<T: serde::de::DeserializeOwned>(s: Option<String>) -> Vec<T> {
    s.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
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
    self_or_owner(&a, &user, "only the owner can see other users' keys")?;
    let keys = app.db.rows(
        "SELECT id, name, prefix, expires_at, allowed_ips, last_used_at, last_used_ip, created_at, revoked_at FROM api_keys WHERE user_id = ?1 ORDER BY created_at DESC",
        [&user],
        |r| {
            Ok(KeyOut {
                id: r.get(0)?,
                name: r.get(1)?,
                prefix: r.get(2)?,
                expires_at: r.get(3)?,
                allowed_ips: parse_ips(r.get(4)?),
                last_used_at: r.get(5)?,
                last_used_ip: r.get(6)?,
                created_at: r.get(7)?,
                revoked_at: r.get(8)?,
            })
        },
    )?;
    Ok(Json(json!({ "keys": keys })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct KeyBody {
    name: Option<String>,
    expires_days: Option<u32>,
    allowed_ips: Vec<ipnet::IpNet>,
}

impl KeyBody {
    fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("default")
    }
    fn expires(&self) -> Option<i64> {
        self.expires_days.map(|d| util::now() + i64::from(d) * 86400)
    }
}

pub async fn create_key(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(user): Path<String>, Json(b): Json<KeyBody>) -> ApiResult<Json<Value>> {
    self_or_owner(&a, &user, "only the owner can make keys for other users")?;
    let name: String = app.db.row("SELECT name FROM users WHERE id = ?1", [&user], |r| r.get(0))?.ok_or_else(|| ApiError::not_found("no such user"))?;
    let mut expires = b.expires();
    let mut ips = b.allowed_ips.clone();
    // A key that makes a key for its own user cannot shed its expiry or address limits that way.
    if let Some(kid) = a.0.key_id.as_deref().filter(|_| user == a.0.user_id) {
        let own = key_or_404(&app, kid)?;
        if let Some(e) = own.expires_at {
            expires = Some(expires.map_or(e, |x| x.min(e)));
        }
        if !own.allowed_ips.is_empty() {
            if ips.is_empty() {
                ips = own.allowed_ips;
            } else if !ips.iter().all(|n| own.allowed_ips.iter().any(|o| o.contains(n))) {
                return Err(ApiError::forbidden("a key can only make keys for addresses it may use itself"));
            }
        }
    }
    let k = issue_key(&app, &a, &user, b.name(), expires, &ips, &name)?;
    Ok(key_reply(&headers, k))
}

fn issue_key(app: &AppState, a: &Auth, user_id: &str, key_name: &str, expires: Option<i64>, ips: &[ipnet::IpNet], user_name: &str) -> ApiResult<auth::NewKey> {
    let k = auth::create_key(&app.db, user_id, key_name, expires, ips)?;
    a.audit(app, "key.create", Some(&format!("{user_name}/{key_name}")), None);
    Ok(k)
}

fn key_reply(headers: &HeaderMap, k: auth::NewKey) -> Json<Value> {
    Json(json!({ "id": k.id, "token": k.token, "snippets": snippets(&base_url(headers), &k.token) }))
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
pub async fn create_agent(State(app): State<AppState>, Owner(a): Owner, headers: HeaderMap, Json(b): Json<AgentBody>) -> ApiResult<Json<Value>> {
    if b.access.all_projects_role.is_none() && b.access.projects.is_empty() {
        return Err(ApiError::bad_request("give the agent at least one project or a role on all projects"));
    }
    let name = b.name.trim().to_string();
    let user = CreateUserBody { kind: UserKind::Agent, name: b.name, email: None, password: None, description: b.description, access: b.access };
    let id = new_user(&app, &a, user).await?;
    let k = issue_key(&app, &a, &id, b.key.name(), b.key.expires(), &b.key.allowed_ips, &name)?;
    Ok(Json(json!({
        "user": read_user(&app.db, &id)?,
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

struct KeyRow {
    user_id: String,
    name: String,
    expires_at: Option<i64>,
    allowed_ips: Vec<ipnet::IpNet>,
    revoked: bool,
}

fn key_or_404(app: &AppState, key_id: &str) -> ApiResult<KeyRow> {
    app.db
        .row("SELECT user_id, name, expires_at, allowed_ips, revoked_at IS NOT NULL FROM api_keys WHERE id = ?1", [key_id], |r| {
            Ok(KeyRow { user_id: r.get(0)?, name: r.get(1)?, expires_at: r.get(2)?, allowed_ips: parse_ips(r.get(3)?), revoked: r.get(4)? })
        })?
        .ok_or_else(|| ApiError::not_found("no such key"))
}

pub async fn revoke_key(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let k = key_or_404(&app, &id)?;
    self_or_owner(&a, &k.user_id, "only the owner can revoke other users' keys")?;
    app.db.exec("UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![util::now(), id])?;
    a.audit(&app, "key.revoke", Some(&k.name), None);
    Ok(ok())
}

pub async fn rotate_key(State(app): State<AppState>, a: Auth, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let old = key_or_404(&app, &id)?;
    self_or_owner(&a, &old.user_id, "only the owner can rotate other users' keys")?;
    if old.revoked {
        return Err(ApiError::bad_request("this key was revoked; make a new one instead"));
    }
    let k = auth::create_key(&app.db, &old.user_id, &old.name, old.expires_at, &old.allowed_ips)?;
    app.db.exec("UPDATE api_keys SET revoked_at = ?1 WHERE id = ?2", params![util::now(), id])?;
    a.audit(&app, "key.rotate", Some(&old.name), None);
    Ok(key_reply(&headers, k))
}

pub async fn get_settings(State(app): State<AppState>, _: Owner) -> ApiResult<Json<Settings>> {
    Ok(Json(app.settings()))
}

pub async fn update_settings(State(app): State<AppState>, Owner(a): Owner, Json(patch): Json<Value>) -> ApiResult<Json<Settings>> {
    let next: Settings = super::patched(&app.settings(), &patch, "settings")?;
    app.db.set_setting("instance", &next)?;
    a.audit(&app, "settings.update", None, Some(&patch.as_object().map(|o| names(o.keys())).unwrap_or_default()));
    app.emit("settings", None, None, json!({}));
    Ok(Json(next))
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
    } else {
        a.require_servers("pick a project to see its audit log")?;
    }
    // Entries outside projects (sign-ins and their addresses, users, keys, settings) are the owner's.
    let rows = app.db.rows(
        "SELECT id, at, actor_name AS actor, actor_kind, key_id, ip, action, project_id, service_id, target, detail FROM audit
         WHERE (?1 IS NULL OR project_id = ?1) AND (?2 IS NULL OR service_id = ?2) AND (?3 IS NULL OR actor_name = ?3)
           AND (?4 IS NULL OR id < ?4) AND (?6 OR project_id IS NOT NULL) ORDER BY id DESC LIMIT ?5",
        params![q.project, q.service, q.actor, q.before, q.limit.unwrap_or(100).min(500), a.0.is_owner],
        super::row_json,
    )?;
    Ok(Json(json!({ "entries": rows })))
}

pub async fn update_status(_: Owner) -> ApiResult<Json<Value>> {
    let latest = crate::updates::latest().await.ok();
    let available = latest.as_ref().is_some_and(|r| crate::updates::newer(&r.version));
    Ok(Json(json!({ "current": crate::VERSION, "latest": latest, "available": available })))
}

/// Install the newest release. Running deploys finish first; traffic keeps flowing.
pub async fn update_apply(State(app): State<AppState>, Owner(a): Owner) -> ApiResult<Json<Value>> {
    let latest = crate::updates::latest().await.map_err(super::bad)?;
    if !crate::updates::newer(&latest.version) {
        return Err(ApiError::bad_request("Tugport is already up to date"));
    }
    a.audit(&app, "tugport.update_requested", Some(&latest.version), None);
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
    use super::UpdateUserBody;
    use serde_json::json;

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
