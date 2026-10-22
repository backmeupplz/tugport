//! Logs, metrics, Docker disk usage, notification channels and registry credentials.

use super::projects::service_or_404;
use super::{ApiError, ApiResult, Auth};
use crate::app::AppState;
use crate::auth::Role;
use crate::{logs, metrics, util};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::sse::{self, KeepAlive, Sse};
use futures_util::{Stream, StreamExt};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct LogQuery {
    since: Option<i64>,
    until: Option<i64>,
    q: Option<String>,
    replica: Option<String>,
    limit: Option<usize>,
}

pub async fn service_logs(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Query(q): Query<LogQuery>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let query = logs::Query { since: q.since, until: q.until, q: q.q, replica: q.replica, limit: q.limit.unwrap_or(500).min(5000) };
    let app2 = app.clone();
    let sid = s.id.clone();
    let lines = tokio::task::spawn_blocking(move || logs::search(&app2, &sid, &query)).await.map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "lines": lines })))
}

pub async fn service_log_stream(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let sid = s.id.clone();
    let stream = tokio_stream::wrappers::BroadcastStream::new(app.logs.subscribe()).filter_map(move |l| {
        let sid = sid.clone();
        async move {
            let l = l.ok()?;
            (l.service_id == sid).then(|| Ok(sse::Event::default().event("line").data(serde_json::to_string(&l).unwrap_or_default())))
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SelfLogQuery {
    limit: Option<usize>,
    level: Option<String>,
}

/// Tugport's own log (owner only).
pub async fn tugport_logs(State(app): State<AppState>, a: Auth, Query(q): Query<SelfLogQuery>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let order = |l: &str| match l.to_ascii_uppercase().as_str() {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0,
    };
    let min = q.level.as_deref().map(order).unwrap_or(0);
    let limit = q.limit.unwrap_or(500).min(2000);
    let recent = crate::util::lock(&app.selflog.recent).clone();
    let lines: Vec<Value> = recent
        .iter()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| order(v["level"].as_str().unwrap_or("")) >= min)
        .take(limit)
        .map(|v| {
            json!({ "ts": v["timestamp"], "level": v["level"], "target": v["target"],
                    "message": v["fields"]["message"].as_str().map(String::from).unwrap_or_else(|| v["fields"].to_string()) })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok(Json(json!({ "lines": lines })))
}

pub async fn log_usage(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let (total, per) = logs::usage(&app);
    Ok(Json(json!({ "total_bytes": total, "services": per.into_iter().map(|(id, b)| json!({ "id": id, "bytes": b })).collect::<Vec<_>>() })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NodeQuery {
    node: Option<String>,
}

/// The latest snapshot of a server (this one by default).
pub async fn metrics_now(State(app): State<AppState>, a: Auth, Query(q): Query<NodeQuery>) -> ApiResult<Json<Value>> {
    let mut snap = match q.node.as_deref().filter(|n| *n != "local") {
        Some(n) => crate::util::lock(&app.metrics.nodes).get(n).cloned().ok_or_else(|| ApiError::not_found("no metrics from that server yet"))?,
        None => crate::util::lock(&app.metrics.latest).clone(),
    };
    if !a.0.is_owner && a.0.all_projects_role.is_none() {
        // Per-service numbers only for services the caller can see.
        let visible: Vec<String> = crate::store::services(&app, None)?.into_iter().filter(|s| a.0.can(&s.project_id, Role::Viewer)).map(|s| s.id).collect();
        snap.services.retain(|s| visible.contains(&s.id));
    }
    Ok(Json(serde_json::to_value(snap)?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct MetricsQuery {
    scope: Option<String>,
    names: Option<String>,
    range: Option<String>,
    /// Server for host and GPU scopes (default: the panel's own).
    node: Option<String>,
}

pub async fn metrics_query(State(app): State<AppState>, a: Auth, Query(q): Query<MetricsQuery>) -> ApiResult<Json<Value>> {
    let scope = q.scope.unwrap_or_else(|| "host".into());
    if let Some(sid) = scope.strip_prefix("service:") {
        let s = service_or_404(&app, &a, sid, Role::Viewer)?;
        if s.id != sid {
            return Err(ApiError::bad_request("use the service id in metric scopes"));
        }
    } else if !a.0.is_owner && a.0.all_projects_role.is_none() {
        return Err(ApiError::forbidden("server metrics need the owner or a role on all projects"));
    }
    let names: Vec<String> = q.names.map(|n| n.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let range = metrics::parse_range(q.range.as_deref().unwrap_or("1h"));
    let app2 = app.clone();
    let node = q.node.unwrap_or_else(|| "local".into());
    let (step, series) = tokio::task::spawn_blocking(move || metrics::query(&app2, &node, &scope, &names, range)).await.map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "step": step, "series": series })))
}

pub async fn docker_usage(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.all_projects_role.is_none() {
        return Err(ApiError::forbidden("needs the owner or a role on all projects"));
    }
    let df = app.docker.df(None::<bollard::query_parameters::DataUsageOptions>).await.map_err(anyhow::Error::from)?;
    let images: i64 = df.images.iter().flatten().map(|i| i.size).sum();
    let build: i64 = df.build_cache.iter().flatten().filter_map(|b| b.size).sum();
    let volumes: i64 = df.volumes.iter().flatten().filter_map(|v| v.usage_data.as_ref().map(|u| u.size)).sum();
    let containers: i64 = df.containers.iter().flatten().filter_map(|c| c.size_rw).sum();
    Ok(Json(json!({ "images": images, "build_cache": build, "volumes": volumes, "containers": containers })))
}

pub async fn list_channels(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, kind, name, events, enabled, created_at FROM notification_channels ORDER BY created_at")?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?,
                       "events": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or(json!(["*"])),
                       "enabled": r.get::<_, bool>(4)?, "created_at": r.get::<_, i64>(5)? }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "channels": rows })))
}

#[derive(Deserialize)]
pub struct ChannelBody {
    kind: String,
    name: String,
    config: Value,
    #[serde(default)]
    events: Vec<String>,
}

pub async fn create_channel(State(app): State<AppState>, a: Auth, Json(b): Json<ChannelBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let required: &[&str] = match b.kind.as_str() {
        "telegram" => &["bot_token", "chat_id"],
        "email" => &["host", "from", "to"],
        "webhook" => &["url"],
        _ => return Err(ApiError::bad_request("kind is telegram, email or webhook")),
    };
    for k in required {
        if b.config.get(*k).is_none_or(|v| v.is_null()) {
            return Err(ApiError::bad_request(format!("{} needs {k}", b.kind)));
        }
    }
    let events = if b.events.is_empty() { vec!["*".to_string()] } else { b.events };
    let id = crate::notify::save_channel(&app, &b.kind, &b.name, &b.config, &events)?;
    app.audit(Some(&a.0), "notify.create", None, None, Some(&b.name), Some(&b.kind));
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_channel(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    app.db.conn().execute("DELETE FROM notification_channels WHERE id = ?1", [&id])?;
    app.audit(Some(&a.0), "notify.delete", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn test_channel(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let c = crate::notify::channels(&app)?.into_iter().find(|c| c.id == id).ok_or_else(|| ApiError::not_found("no such channel"))?;
    crate::notify::send(&c, "test", "Tugport test notification", "If you can read this, notifications work.", None)
        .await
        .map_err(|e| ApiError::bad_request(format!("sending failed: {e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn list_registries(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    if !a.0.is_owner && a.0.all_projects_role.is_none_or(|r| r < Role::Operator) {
        return Err(ApiError::forbidden("needs the owner or an operator role on all projects"));
    }
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, host, username FROM registries ORDER BY host")?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| Ok(json!({ "id": r.get::<_, String>(0)?, "host": r.get::<_, String>(1)?, "username": r.get::<_, String>(2)? })))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({ "registries": rows })))
}

#[derive(Deserialize)]
pub struct RegistryBody {
    host: String,
    username: String,
    password: String,
}

pub async fn create_registry(State(app): State<AppState>, a: Auth, Json(b): Json<RegistryBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let host = b.host.trim().trim_start_matches("https://").trim_end_matches('/').to_string();
    let id = util::id();
    app.db.conn().execute(
        "INSERT INTO registries (id, host, username, password, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(host) DO UPDATE SET username = excluded.username, password = excluded.password",
        params![id, host, b.username, app.keys.seal_str(&format!("registry:{host}"), &b.password)?, util::now()],
    )?;
    app.audit(Some(&a.0), "registry.save", None, None, Some(&host), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_registry(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    app.db.conn().execute("DELETE FROM registries WHERE id = ?1 OR host = ?1", [&id])?;
    app.audit(Some(&a.0), "registry.delete", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn tunnel_status(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    Ok(Json(crate::tunnel::status(&app).await?))
}

#[derive(Deserialize)]
pub struct TunnelBody {
    api_token: String,
}

pub async fn tunnel_connect(State(app): State<AppState>, a: Auth, Json(b): Json<TunnelBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    if crate::tunnel::load(&app)?.is_some() {
        return Err(ApiError::conflict("a tunnel is already connected; disconnect it first"));
    }
    crate::tunnel::connect(&app, b.api_token.trim()).await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    app.audit(Some(&a.0), "tunnel.connect", None, None, None, None);
    Ok(Json(crate::tunnel::status(&app).await?))
}

pub async fn tunnel_disconnect(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    crate::tunnel::disconnect(&app).await?;
    app.audit(Some(&a.0), "tunnel.disconnect", None, None, None, None);
    Ok(Json(json!({ "ok": true })))
}
