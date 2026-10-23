//! Servers: list, add (join command), rename/label, remove.

use super::{ApiError, ApiResult, Auth, base_url};
use crate::app::AppState;
use crate::nodes;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn require_servers(a: &Auth) -> ApiResult<()> {
    if a.0.is_owner || a.0.all_projects_role.is_some() { Ok(()) } else { Err(ApiError::forbidden("servers need the owner or a role on all projects")) }
}

pub async fn list(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    require_servers(&a)?;
    let mesh = nodes::mesh_enabled(&app);
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, name, is_local, address, mesh_ip, region, labels, status, info, last_seen_at, created_at FROM nodes ORDER BY is_local DESC, name")?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            let labels: String = r.get(6)?;
            let info: String = r.get(8)?;
            Ok(json!({
                "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?, "local": r.get::<_, bool>(2)?,
                "address": r.get::<_, Option<String>>(3)?, "mesh_ip": r.get::<_, Option<String>>(4)?, "region": r.get::<_, Option<String>>(5)?,
                "labels": serde_json::from_str::<Value>(&labels).unwrap_or_default(), "status": r.get::<_, String>(7)?,
                "info": serde_json::from_str::<Value>(&info).unwrap_or_default(), "last_seen_at": r.get::<_, Option<i64>>(9)?,
                "created_at": r.get::<_, i64>(10)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Json(json!({ "nodes": rows, "mesh": mesh })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct TokenBody {
    /// Address servers reach the panel on; defaults to the one this request used.
    panel_url: Option<String>,
}

/// A one-hour command that adds a server (turning on the mesh the first time).
pub async fn join_token(State(app): State<AppState>, a: Auth, headers: HeaderMap, body: Option<Json<TokenBody>>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let url = body.and_then(|b| b.0.panel_url).unwrap_or_else(|| base_url(&headers));
    let t = nodes::join_token(&app, url.trim_end_matches('/')).await?;
    app.audit(Some(&a.0), "node.join_token", None, None, None, None);
    Ok(Json(serde_json::to_value(t)?))
}

/// Called by `tugport install --join` on the new server; the one-time token is the credential.
pub async fn join(State(app): State<AppState>, Json(req): Json<nodes::JoinRequest>) -> ApiResult<Json<Value>> {
    match nodes::join(&app, req).await {
        Ok(r) => Ok(Json(serde_json::to_value(r)?)),
        Err(e) => Err(ApiError::bad_request(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
pub struct UpdateBody {
    name: Option<String>,
    /// null or "" clears it.
    #[serde(default, deserialize_with = "super::present")]
    region: Option<Option<String>>,
    labels: Option<BTreeMap<String, String>>,
}

pub async fn update(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<UpdateBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    if app.node(&id).is_none() {
        return Err(ApiError::not_found("server not found"));
    }
    if let Some(name) = &b.name {
        if !crate::util::valid_name(name) {
            return Err(ApiError::bad_request("use lowercase letters, digits and dashes"));
        }
        app.db.conn().execute("UPDATE nodes SET name = ?1 WHERE id = ?2", params![name, id])?;
    }
    if let Some(region) = &b.region {
        let region = region.as_deref().map(str::trim).filter(|r| !r.is_empty());
        app.db.conn().execute("UPDATE nodes SET region = ?1 WHERE id = ?2", params![region, id])?;
    }
    if let Some(labels) = &b.labels {
        app.db.conn().execute("UPDATE nodes SET labels = ?1 WHERE id = ?2", params![serde_json::to_string(labels)?, id])?;
    }
    nodes::load(&app)?;
    app.audit(Some(&a.0), "node.update", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
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
pub async fn remove(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Query(q): Query<RemoveQuery>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let node = app.node(&id).ok_or_else(|| ApiError::not_found("server not found"))?;
    if node.local {
        return Err(ApiError::bad_request("the panel's own server cannot be removed"));
    }
    let name = node.name.clone();
    let keeping: Vec<String> = crate::store::services(&app, None)?
        .into_iter()
        .filter(|s| app.db.get_state(&format!("pin:{}", s.id)).ok().flatten().as_deref() == Some(id.as_str()))
        .map(|s| format!("{}/{}", s.project, s.name))
        .collect();
    if !keeping.is_empty() && !q.force {
        return Err(ApiError::conflict(format!("{name} keeps the data of {}; move or delete them first (or remove with force)", keeping.join(", "))));
    }
    let machine = crate::cloud::machine_of(&app, &id);
    // Finishes even if the caller goes away: a half-drained server would take no new work.
    let (app2, id2, m2) = (app.clone(), id.clone(), machine.clone());
    let done = tokio::spawn(async move {
        if nodes::online(&app2, &id2) {
            nodes::drain(&app2, &id2).await?;
        }
        nodes::remove(&app2, &id2).await?;
        match &m2 {
            Some(m) if q.keep_machine => {
                app2.db.conn().execute("DELETE FROM cloud_servers WHERE id = ?1", [m])?;
            }
            Some(m) => crate::cloud::destroy(&app2, m).await?,
            None => {}
        }
        anyhow::Ok(())
    });
    done.await.map_err(anyhow::Error::from)??;
    app.audit(Some(&a.0), "node.remove", None, None, Some(&name), machine.as_ref().map(|_| if q.keep_machine { "machine kept" } else { "machine deleted" }));
    Ok(Json(json!({ "ok": true, "machine_deleted": machine.is_some() && !q.keep_machine })))
}
