mod access;
mod backups;
mod console;
mod deploys;
mod git;
mod hooks;
mod nodes;
mod observe;
pub mod projects;

use crate::app::AppState;
use crate::auth::{self, KeyCheck, Principal};
use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use std::net::{IpAddr, SocketAddr};

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/api/setup", get(access::setup_status).post(access::setup))
        .route("/api/auth/login", post(access::login))
        .route("/api/auth/logout", post(access::logout))
        .route("/api/me", get(access::me))
        .route("/api/me/totp", post(access::totp_begin).put(access::totp_confirm).delete(access::totp_disable))
        .route("/api/about", get(access::about))
        .route("/api/updates", get(access::update_status))
        .route("/api/updates/apply", post(access::update_apply))
        .route("/api/users", get(access::list_users).post(access::create_user))
        .route("/api/users/{id}", get(access::get_user).patch(access::update_user).delete(access::delete_user))
        .route("/api/users/{id}/keys", get(access::list_keys).post(access::create_key))
        .route("/api/agents", post(access::create_agent))
        .route("/api/keys/{id}", delete(access::revoke_key))
        .route("/api/keys/{id}/rotate", post(access::rotate_key))
        .route("/api/settings", get(access::get_settings).patch(access::update_settings))
        .route("/api/audit", get(access::audit))
        .route("/api/projects", get(projects::list_projects).post(projects::create_project))
        .route("/api/projects/{id}", get(projects::get_project).patch(projects::update_project).delete(projects::delete_project))
        .route("/api/projects/{id}/vars", get(projects::get_vars).put(projects::put_vars))
        .route("/api/projects/{id}/members", get(projects::list_members))
        .route("/api/projects/{id}/members/{user}", put(projects::set_member))
        .route("/api/projects/{id}/services", post(projects::create_service))
        .route("/api/services", get(projects::list_services))
        .route("/api/services/{id}", get(projects::get_service).patch(projects::update_service).delete(projects::delete_service))
        .route("/api/services/{id}/env", get(projects::get_env).put(projects::put_env))
        .route("/api/services/{id}/files", get(projects::list_files).put(projects::put_file))
        .route("/api/services/{id}/domains", get(projects::list_domains).post(projects::create_domain))
        .route("/api/services/{id}/hook", get(projects::get_hook).post(projects::rotate_hook))
        .route("/api/services/{id}/deploy-key", post(git::deploy_key))
        .route("/api/services/{id}/deploy", post(deploys::deploy))
        .route("/api/services/{id}/deploys", get(deploys::list))
        .route("/api/services/{id}/rollback", post(deploys::rollback))
        .route("/api/services/{id}/restart", post(deploys::restart))
        .route("/api/services/{id}/stop", post(deploys::stop))
        .route("/api/services/{id}/start", post(deploys::start))
        .route("/api/domains/{id}", patch(projects::update_domain).delete(projects::delete_domain))
        .route("/api/deploys/{id}", get(deploys::get_one))
        .route("/api/deploys/{id}/log", get(deploys::log_stream))
        .route("/api/deploys/{id}/cancel", post(deploys::cancel))
        .route("/api/events", get(deploys::events))
        .route("/api/services/{id}/logs", get(observe::service_logs))
        .route("/api/services/{id}/logs/stream", get(observe::service_log_stream))
        .route("/api/services/{id}/console", get(console::console))
        .route("/api/services/{id}/exec", post(console::exec))
        .route("/mcp", post(crate::mcp::handle))
        .route("/api/logs/tugport", get(observe::tugport_logs))
        .route("/api/logs/usage", get(observe::log_usage))
        .route("/api/metrics", get(observe::metrics_query))
        .route("/api/metrics/now", get(observe::metrics_now))
        .route("/api/docker/usage", get(observe::docker_usage))
        .route("/api/nodes", get(nodes::list))
        .route("/api/nodes/join-token", post(nodes::join_token))
        .route("/api/nodes/join", post(nodes::join))
        .route("/api/nodes/{id}", patch(nodes::update).delete(nodes::remove))
        .route("/api/notifications", get(observe::list_channels).post(observe::create_channel))
        .route("/api/notifications/{id}", delete(observe::delete_channel))
        .route("/api/notifications/{id}/test", post(observe::test_channel))
        .route("/api/registries", get(observe::list_registries).post(observe::create_registry))
        .route("/api/registries/{id}", delete(observe::delete_registry))
        .route("/api/backups/targets", get(backups::list_targets).post(backups::create_target))
        .route("/api/backups/targets/{id}", delete(backups::delete_target))
        .route("/api/backups/jobs", get(backups::list_jobs).post(backups::create_job))
        .route("/api/backups/jobs/{id}", patch(backups::update_job).delete(backups::delete_job))
        .route("/api/backups/jobs/{id}/run", post(backups::run_job))
        .route("/api/backups/jobs/{id}/runs", get(backups::list_runs))
        .route("/api/backups/runs/{id}/restore", post(backups::restore))
        .route("/api/tunnel", get(observe::tunnel_status).put(observe::tunnel_connect).delete(observe::tunnel_disconnect))
        .route("/api/git/providers", get(git::list).post(git::create))
        .route("/api/git/providers/{id}", delete(git::delete))
        .route("/api/git/providers/{id}/repos", get(git::repos))
        .route("/api/git/github/manifest", post(git::github_manifest))
        .route("/api/git/github/callback", get(git::github_callback))
        .route("/api/hooks/deploy/{token}", post(hooks::deploy_hook).get(hooks::deploy_hook))
        .route("/api/hooks/git/{provider}", post(hooks::git_hook))
        // Easypanel's deploy webhook path, so migrated repositories keep deploying.
        .route("/api/deploy/{token}", post(hooks::deploy_hook).get(hooks::deploy_hook))
        .route("/api/compose/deploy/{token}", post(hooks::deploy_hook).get(hooks::deploy_hook))
        .fallback(crate::web::static_handler)
        .layer(axum::middleware::from_fn(json_errors))
        .with_state(app)
}

/// Axum answers malformed bodies with plain text; API clients get `{"error": ...}` like everywhere else.
async fn json_errors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let api = req.uri().path().starts_with("/api/");
    let resp = next.run(req).await;
    let status = resp.status();
    let plain = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("text/plain"));
    if !api || !plain || !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap_or_default();
    ApiError::new(status, String::from_utf8_lossy(&body).trim()).into_response()
}

/// Only the deploy webhooks, for listeners exposed to the internet.
pub fn hooks_router(app: AppState) -> Router {
    Router::new()
        .route("/api/hooks/deploy/{token}", post(hooks::deploy_hook).get(hooks::deploy_hook))
        .route("/api/hooks/git/{provider}", post(hooks::git_hook))
        .route("/api/deploy/{token}", post(hooks::deploy_hook).get(hooks::deploy_hook))
        .route("/api/compose/deploy/{token}", post(hooks::deploy_hook).get(hooks::deploy_hook))
        .with_state(app)
}

/// For `Option<Option<T>>` body fields: tells null (`Some(None)`, clear it) from a
/// field left out (`None`, keep it). Use with `#[serde(default, deserialize_with = "present")]`.
pub fn present<'de, D: serde::Deserializer<'de>, T: serde::Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError { status, message: message.into() }
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, m)
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, m)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e
            && f.code == rusqlite::ErrorCode::ConstraintViolation {
                return ApiError::conflict("already exists");
            }
        anyhow::Error::from(e).into()
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

/// The client's address. Requests that arrive through our own proxy (loopback)
/// carry the real address in the last X-Forwarded-For entry.
pub fn client_ip(parts: &Parts) -> Option<IpAddr> {
    let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_canonical())?;
    if peer.is_loopback()
        && let Some(xff) = parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(last) = xff.rsplit(',').next().and_then(|s| s.trim().parse().ok()) {
                return Some(last);
            }
    Some(peer)
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// An authenticated caller: an API key (`Authorization: Bearer tpk_...`) or a session cookie.
pub struct Auth(pub Principal);

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Self::Rejection> {
        let ip = client_ip(parts);
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        let (user_id, key_id) = if let Some(token) = bearer {
            match auth::check_key(&app.db, token, ip)? {
                KeyCheck::Ok { user_id, key_id } => (user_id, Some(key_id)),
                KeyCheck::Expired => return Err(ApiError::unauthorized("this API key has expired")),
                KeyCheck::Revoked => return Err(ApiError::unauthorized("this API key was revoked")),
                KeyCheck::IpNotAllowed => return Err(ApiError::forbidden("this API key is not allowed from your address")),
                KeyCheck::Unknown => return Err(ApiError::unauthorized("unknown API key")),
            }
        } else if let Some(token) = cookie(&parts.headers, auth::SESSION_COOKIE) {
            match auth::session_user(&app.db, &token)? {
                Some(uid) => (uid, None),
                None => return Err(ApiError::unauthorized("session expired, sign in again")),
            }
        } else {
            return Err(ApiError::unauthorized("sign in or send an API key"));
        };
        let mut p = auth::load_principal(&app.db, &user_id)?.ok_or_else(|| ApiError::unauthorized("user is disabled"))?;
        p.key_id = key_id;
        p.ip = ip.map(|i| i.to_string());
        Ok(Auth(p))
    }
}

impl Auth {
    pub fn require_owner(&self) -> ApiResult<()> {
        if self.0.is_owner { Ok(()) } else { Err(ApiError::forbidden("only the owner can do this")) }
    }
    pub fn require(&self, project_id: &str, role: auth::Role) -> ApiResult<()> {
        if self.0.can(project_id, role) {
            Ok(())
        } else if self.0.role_in(project_id).is_none() {
            Err(ApiError::not_found("project not found"))
        } else {
            Err(ApiError::forbidden(format!("this needs the {} role on the project", role.as_str())))
        }
    }
}

pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, Self::Rejection> {
        Ok(ClientIp(client_ip(parts)))
    }
}

/// Base URL the caller used to reach the panel (for copy-paste snippets).
pub fn base_url(headers: &HeaderMap) -> String {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost:8484");
    let proto = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    format!("{proto}://{host}")
}
