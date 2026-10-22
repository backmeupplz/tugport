mod access;

use crate::app::AppState;
use crate::auth::{self, KeyCheck, Principal};
use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use std::net::{IpAddr, SocketAddr};

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/api/setup", get(access::setup_status).post(access::setup))
        .route("/api/auth/login", post(access::login))
        .route("/api/auth/logout", post(access::logout))
        .route("/api/me", get(access::me))
        .route("/api/me/totp", post(access::totp_begin).put(access::totp_confirm).delete(access::totp_disable))
        .route("/api/about", get(access::about))
        .route("/api/users", get(access::list_users).post(access::create_user))
        .route("/api/users/{id}", get(access::get_user).patch(access::update_user).delete(access::delete_user))
        .route("/api/users/{id}/keys", get(access::list_keys).post(access::create_key))
        .route("/api/agents", post(access::create_agent))
        .route("/api/keys/{id}", delete(access::revoke_key))
        .route("/api/keys/{id}/rotate", post(access::rotate_key))
        .route("/api/settings", get(access::get_settings).patch(access::update_settings))
        .route("/api/audit", get(access::audit))
        .fallback(crate::web::static_handler)
        .with_state(app)
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
        if let rusqlite::Error::SqliteFailure(f, _) = &e {
            if f.code == rusqlite::ErrorCode::ConstraintViolation {
                return ApiError::conflict("already exists");
            }
        }
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
    let peer = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip())?;
    if peer.is_loopback() {
        if let Some(xff) = parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.rsplit(',').next().and_then(|s| s.trim().parse().ok()) {
                return Some(last);
            }
        }
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
