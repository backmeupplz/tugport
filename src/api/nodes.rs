//! Servers: list, add (join command), rename/label, remove.

use super::{bad, detached};
use super::{ApiError, ApiResult, Auth, Owner, base_url, ok};
use crate::app::AppState;
use crate::nodes;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub async fn list(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_servers("servers need the owner or a role on all projects")?;
    let mesh = nodes::mesh_enabled(&app);
    let sql = "SELECT id, name, is_local, address, mesh_ip, region, labels, status, info, last_seen_at, created_at, ingress FROM nodes ORDER BY is_local DESC, name";
    let json_col = |r: &rusqlite::Row, i| r.get::<_, String>(i).map(|t| serde_json::from_str::<Value>(&t).unwrap_or_default());
    let rows = app.db.rows(sql, [], |r| {
        let local: bool = r.get(2)?;
        Ok(json!({
            "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "local": local,
            "address": r.get::<_, Option<String>>(3)?, "mesh_ip": r.get::<_, Option<String>>(4)?, "region": r.get::<_, Option<String>>(5)?,
            "labels": json_col(r, 6)?, "status": r.get::<_, String>(7)?, "info": json_col(r, 8)?, "last_seen_at": r.get::<_, Option<i64>>(9)?,
            "created_at": r.get::<_, i64>(10)?, "ingress": local || r.get::<_, bool>(11)?,
        }))
    })?;
    Ok(Json(json!({ "nodes": rows, "mesh": mesh })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TokenBody {
    /// Address servers reach the panel on; defaults to the one this request used.
    panel_url: Option<String>,
}

/// A one-hour command that adds a server (turning on the mesh the first time).
pub async fn join_token(State(app): State<AppState>, Owner(a): Owner, headers: HeaderMap, body: Option<Json<TokenBody>>) -> ApiResult<Json<Value>> {
    let url = body.and_then(|b| b.0.panel_url).unwrap_or_else(|| base_url(&headers));
    let t = nodes::join_token_for(&app, url.trim_end_matches('/'), None).await?;
    a.audit(&app, "node.join_token", None, None);
    Ok(Json(serde_json::to_value(t)?))
}

/// Called by `tugport install --join` on the new server; the one-time token is the credential.
pub async fn join(State(app): State<AppState>, super::ClientIp(seen): super::ClientIp, headers: HeaderMap, Json(req): Json<nodes::JoinRequest>) -> ApiResult<Json<Value>> {
    // The panel address this server reached us at, when it is a plain IP.
    let host = headers.get(axum::http::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let panel_ip = host.parse::<std::net::SocketAddr>().map(|a| a.ip()).or_else(|_| host.trim_matches(['[', ']']).parse()).ok();
    let r = nodes::join(&app, req, panel_ip, seen).await.map_err(bad)?;
    Ok(Json(serde_json::to_value(r)?))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    name: Option<String>,
    /// null or "" clears it.
    #[serde(default, deserialize_with = "super::present")]
    region: Option<Option<String>>,
    labels: Option<BTreeMap<String, String>>,
    /// Let this server terminate public traffic (point DNS at it). Off by default, so a plain
    /// worker holds no certificates or auth config.
    ingress: Option<bool>,
}

pub async fn update(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>, Json(b): Json<UpdateBody>) -> ApiResult<Json<Value>> {
    if app.node(&id).is_none() {
        return Err(ApiError::not_found("server not found"));
    }
    if let Some(name) = &b.name {
        if !crate::util::valid_name(name) {
            return Err(ApiError::bad_request("use lowercase letters, digits and dashes"));
        }
        app.db.exec("UPDATE nodes SET name = ?1 WHERE id = ?2", params![name, id])?;
    }
    if let Some(region) = &b.region {
        let region = region.as_deref().map(str::trim).filter(|r| !r.is_empty());
        app.db.exec("UPDATE nodes SET region = ?1 WHERE id = ?2", params![region, id])?;
    }
    if let Some(labels) = &b.labels {
        app.db.exec("UPDATE nodes SET labels = ?1 WHERE id = ?2", params![serde_json::to_string(labels)?, id])?;
    }
    if let Some(ingress) = b.ingress {
        // The panel is always ingress; this toggles the other servers.
        app.db.exec("UPDATE nodes SET ingress = ?1 WHERE id = ?2 AND is_local = 0", params![ingress, id])?;
    }
    nodes::load(&app)?;
    if b.ingress.is_some() {
        // A server turned into an ingress needs the certificates and the full table; one turned
        // back loses them on its next pushed table (the old ones linger until it restarts).
        nodes::push_certs(&app).await;
        nodes::push_routes(&app);
    }
    a.audit(&app, "node.update", Some(&id), None);
    Ok(ok())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RemoveQuery {
    /// Keep the cloud machine running (Tugport created it, so by default it is deleted too).
    keep_machine: bool,
    /// Remove it even though services keep data there (that data stays on the machine).
    force: bool,
}

/// Remove a server. Movable services go to other servers first; a machine Tugport created
/// in a cloud account is deleted there as well.
pub async fn remove(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>, Query(q): Query<RemoveQuery>) -> ApiResult<Json<Value>> {
    let node = app.node(&id).ok_or_else(|| ApiError::not_found("server not found"))?;
    if node.local {
        return Err(ApiError::bad_request("the panel's own server cannot be removed"));
    }
    let keeping: Vec<String> = crate::store::services(&app, None)?
        .into_iter()
        .filter(|s| app.db.state::<String>(&format!("pin:{}", s.id)).as_deref() == Some(id.as_str()))
        .map(|s| format!("{}/{}", s.project, s.name))
        .collect();
    if !keeping.is_empty() && !q.force {
        return Err(ApiError::conflict(format!("{} keeps the data of {}; move or delete them first (or remove with force)", node.name, keeping.join(", "))));
    }
    let machine = crate::cloud::machine_of(&app, &id);
    // Finishes even if the caller goes away: a half-drained server would take no new work.
    let (app2, id2, m2) = (app.clone(), id.clone(), machine.clone());
    detached(async move {
        if nodes::online(&app2, &id2) {
            nodes::drain(&app2, &id2).await?;
        }
        nodes::remove(&app2, &id2).await?;
        match &m2 {
            Some(m) if q.keep_machine => {
                app2.db.exec("DELETE FROM cloud_servers WHERE id = ?1", [m])?;
            }
            Some(m) => crate::cloud::destroy(&app2, m).await?,
            None => {}
        }
        anyhow::Ok(())
    })
    .await??;
    a.audit(&app, "node.remove", Some(&node.name), machine.as_ref().map(|_| if q.keep_machine { "machine kept" } else { "machine deleted" }));
    Ok(Json(json!({ "ok": true, "machine_deleted": machine.is_some() && !q.keep_machine })))
}
