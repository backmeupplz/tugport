//! Logs, metrics, Docker disk usage, notification channels and registry credentials.

use super::service_or_404;
use super::{ApiError, ApiResult, Auth, Events, Owner, ok, sse_events, sse_json};
use crate::app::AppState;
use crate::auth::Role;
use crate::{logs, metrics, util};
use axum::Json;
use axum::extract::{Path, Query, State};
use futures_util::StreamExt;
use futures_util::future::ready;
use rusqlite::params;
use super::{bad, detached, row_json};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

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
    let lines = tokio::task::spawn_blocking(move || logs::search(&app, &s.id, &query)).await.map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "lines": lines })))
}

pub async fn service_log_stream(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Events> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let stream = util::subscribe(app.logs.subscribe()).filter(move |l| ready(l.service_id == s.id)).map(|l| sse_json("line", &l));
    Ok(sse_events(stream))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SelfLogQuery {
    limit: Option<usize>,
    level: Option<String>,
}

/// Tugport's own log (owner only).
pub async fn tugport_logs(State(app): State<AppState>, _: Owner, Query(q): Query<SelfLogQuery>) -> ApiResult<Json<Value>> {
    let order = |l: &str| match l.to_ascii_uppercase().as_str() {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0,
    };
    let min = q.level.as_deref().map(order).unwrap_or(0);
    let limit = q.limit.unwrap_or(500).min(2000);
    let recent = util::lock(&app.selflog.recent).clone();
    let mut lines: Vec<Value> = recent
        .iter()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| order(v["level"].as_str().unwrap_or("")) >= min)
        .take(limit)
        .map(|v| {
            json!({ "ts": v["timestamp"], "level": v["level"], "target": v["target"],
                    "message": v["fields"]["message"].as_str().map(String::from).unwrap_or_else(|| v["fields"].to_string()) })
        })
        .collect();
    lines.reverse();
    Ok(Json(json!({ "lines": lines })))
}

pub async fn log_usage(State(app): State<AppState>, _: Owner) -> ApiResult<Json<Value>> {
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
        Some(n) => util::lock(&app.metrics.nodes).get(n).cloned().ok_or_else(|| ApiError::not_found("no metrics from that server yet"))?,
        None => util::lock(&app.metrics.latest).clone(),
    };
    if !a.0.sees_servers() {
        // Project members see their services' numbers, not the servers'.
        let visible: std::collections::HashSet<String> = super::visible_services(&app, &a)?.into_iter().map(|s| s.id).collect();
        snap.services.retain(|s| visible.contains(&s.id));
        return Ok(Json(json!({ "at": snap.at, "services": snap.services })));
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
    } else {
        a.require_servers("server metrics need the owner or a role on all projects")?;
    }
    let names: Vec<String> = q.names.map(|n| n.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let range = metrics::parse_range(q.range.as_deref().unwrap_or("1h"));
    let node = q.node.unwrap_or_else(|| "local".into());
    let (step, series) = tokio::task::spawn_blocking(move || metrics::query(&app, &node, &scope, &names, range)).await.map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "step": step, "series": series })))
}

pub async fn docker_usage(State(app): State<AppState>, a: Auth, Query(q): Query<NodeQuery>) -> ApiResult<Json<Value>> {
    a.require_servers("needs the owner or a role on all projects")?;
    let node = q.node.as_deref().unwrap_or("local");
    let docker = app.docker_on(node).map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    Ok(Json(util::cached(&DOCKER_USAGE, node, Duration::from_secs(300), docker_df(docker)).await?))
}

/// Docker's disk usage takes seconds (it sizes every volume), so it is kept for a few minutes.
static DOCKER_USAGE: util::Cache<Value> = std::sync::Mutex::new(std::collections::BTreeMap::new());

async fn docker_df(docker: bollard::Docker) -> anyhow::Result<Value> {
    let df = docker.df(None::<bollard::query_parameters::DataUsageOptions>).await?;
    let images: i64 = df.images.iter().flatten().map(|i| i.size).sum();
    let build: i64 = df.build_cache.iter().flatten().filter_map(|b| b.size).sum();
    let volumes: i64 = df.volumes.iter().flatten().filter_map(|v| v.usage_data.as_ref().map(|u| u.size)).sum();
    let containers: i64 = df.containers.iter().flatten().filter_map(|c| c.size_rw).sum();
    Ok(json!({ "images": images, "build_cache": build, "volumes": volumes, "containers": containers }))
}

/// Measure this server's Docker disk usage in the background, so the first look is instant too.
pub async fn warm_docker_usage(app: AppState) {
    let _ = util::cached(&DOCKER_USAGE, "local", Duration::ZERO, docker_df(app.docker.clone())).await;
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RequestQuery {
    route: Option<String>,
    method: Option<String>,
    who: Option<String>,
    /// `5xx`, `4xx`, ... or an exact status.
    status: Option<String>,
    min_ms: Option<f64>,
    /// Only the last this many hours.
    hours: Option<i64>,
    /// Slowest first instead of newest first.
    slowest: bool,
    before: Option<i64>,
    limit: Option<i64>,
}

/// Logged API requests, newest (or slowest) first.
pub async fn requests(State(app): State<AppState>, _: Owner, Query(q): Query<RequestQuery>) -> ApiResult<Json<Value>> {
    let (lo, hi) = match q.status.as_deref() {
        None | Some("") => (0, 999),
        Some(s) => match (s.strip_suffix("xx").and_then(|d| d.parse::<u16>().ok()), s.parse::<u16>().ok()) {
            (Some(d), _) => (d * 100, d * 100 + 99),
            (None, Some(n)) => (n, n),
            _ => return Err(ApiError::bad_request("status is like 5xx or 404")),
        },
    };
    let since = q.hours.map_or(0, |h| util::now_ms() - h.clamp(1, 24 * 7) * 3_600_000);
    let order = if q.slowest { "us DESC" } else { "id DESC" };
    let sql = format!(
        "SELECT id, at, method, route, path, query, status, us, bytes, who, key_id, ip, agent FROM requests
         WHERE (?1 IS NULL OR route = ?1) AND (?2 IS NULL OR method = ?2) AND (?3 IS NULL OR who = ?3) AND status BETWEEN ?4 AND ?5
           AND us >= ?6 AND at >= ?7 AND (?8 IS NULL OR id < ?8)
         ORDER BY {order} LIMIT ?9"
    );
    let min_us = (q.min_ms.unwrap_or(0.0) * 1000.0) as i64;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let rows = crate::requests::read(&app, move |c| {
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(params![q.route, q.method, q.who, lo, hi, min_us, since, q.before, limit], row_json)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await?;
    Ok(Json(json!({ "entries": rows })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SummaryQuery {
    hours: Option<i64>,
}

/// Per route over the last hours (24 by default): calls, server errors and time percentiles.
pub async fn request_summary(State(app): State<AppState>, _: Owner, Query(q): Query<SummaryQuery>) -> ApiResult<Json<Value>> {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 7);
    let since = util::now_ms() - hours * 3_600_000;
    let routes = crate::requests::read(&app, move |c| crate::requests::summary(c, since)).await?;
    Ok(Json(json!({ "hours": hours, "routes": routes })))
}

pub async fn list_channels(State(app): State<AppState>, _: Owner) -> ApiResult<Json<Value>> {
    let rows = app.db.rows("SELECT id, kind, name, events, enabled, created_at FROM notification_channels ORDER BY created_at", [], |r| {
        Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?,
                   "events": serde_json::from_str::<Value>(&r.get::<_, String>(3)?).unwrap_or(json!(["*"])),
                   "enabled": r.get::<_, bool>(4)?, "created_at": r.get::<_, i64>(5)? }))
    })?;
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

pub async fn create_channel(State(app): State<AppState>, Owner(a): Owner, Json(b): Json<ChannelBody>) -> ApiResult<Json<Value>> {
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
    a.audit(&app, "notify.create", Some(&b.name), Some(&b.kind));
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_channel(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.db.exec("DELETE FROM notification_channels WHERE id = ?1", [&id])?;
    a.audit(&app, "notify.delete", Some(&id), None);
    Ok(ok())
}

pub async fn test_channel(State(app): State<AppState>, _: Owner, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let c = crate::notify::channels(&app)?.into_iter().find(|c| c.id == id).ok_or_else(|| ApiError::not_found("no such channel"))?;
    crate::notify::send(&c, "test", "Tugport test notification", "If you can read this, notifications work.", None)
        .await
        .map_err(|e| ApiError::bad_request(format!("sending failed: {e:#}")))?;
    Ok(ok())
}

pub async fn list_registries(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    if !a.operates_all_projects() {
        return Err(ApiError::forbidden("needs the owner or an operator role on all projects"));
    }
    let rows = app.db.rows("SELECT id, host, username FROM registries ORDER BY host", [], row_json)?;
    Ok(Json(json!({ "registries": rows })))
}

#[derive(Deserialize)]
pub struct RegistryBody {
    host: String,
    username: String,
    password: String,
}

pub async fn create_registry(State(app): State<AppState>, Owner(a): Owner, Json(b): Json<RegistryBody>) -> ApiResult<Json<Value>> {
    let host = b.host.trim().trim_start_matches("https://").trim_end_matches('/').to_string();
    let id = util::id();
    app.db.exec(
        "INSERT INTO registries (id, host, username, password, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(host) DO UPDATE SET username = excluded.username, password = excluded.password",
        params![id, host, b.username, app.keys.seal_str(&format!("registry:{host}"), &b.password)?, util::now()],
    )?;
    a.audit(&app, "registry.save", Some(&host), None);
    Ok(ok())
}

pub async fn delete_registry(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    app.db.exec("DELETE FROM registries WHERE id = ?1 OR host = ?1", [&id])?;
    a.audit(&app, "registry.delete", Some(&id), None);
    Ok(ok())
}

pub async fn tunnel_status(State(app): State<AppState>, _: Owner) -> ApiResult<Json<Value>> {
    Ok(Json(crate::tunnel::status(&app).await?))
}

#[derive(Deserialize)]
pub struct TunnelBody {
    api_token: String,
}

pub async fn tunnel_connect(State(app): State<AppState>, Owner(a): Owner, Json(b): Json<TunnelBody>) -> ApiResult<Json<Value>> {
    if crate::tunnel::load(&app)?.is_some() {
        return Err(ApiError::conflict("a tunnel is already connected; disconnect it first"));
    }
    // Detached, so a dropped request can't stop it halfway with a tunnel created at Cloudflare.
    let (app2, token) = (app.clone(), b.api_token.trim().to_string());
    detached(async move { crate::tunnel::connect(&app2, &token).await }).await?.map_err(bad)?;
    a.audit(&app, "tunnel.connect", None, None);
    Ok(Json(crate::tunnel::status(&app).await?))
}

pub async fn tunnel_disconnect(State(app): State<AppState>, Owner(a): Owner) -> ApiResult<Json<Value>> {
    let app2 = app.clone();
    detached(async move { crate::tunnel::disconnect(&app2).await }).await??;
    a.audit(&app, "tunnel.disconnect", None, None);
    Ok(ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_become_objects() {
        let db = crate::db::Db::memory().unwrap();
        let rows = db.rows("SELECT 'a' AS id, 7 AS n, NULL AS gone, 1.5 AS f", [], super::row_json).unwrap();
        assert_eq!(rows, vec![serde_json::json!({ "id": "a", "n": 7, "gone": null, "f": 1.5 })]);
    }
}
