//! Cloud accounts, servers created in them, and server pools. Owner only: these spend money.

use super::{ApiError, ApiResult, Auth};
use crate::app::AppState;
use crate::cloud::{self, Credentials};
use crate::util;
use axum::Json;
use axum::extract::{Path, State};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub async fn list_accounts(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, kind, name, created_at FROM cloud_accounts ORDER BY name")?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?, "created_at": r.get::<_, i64>(3)? })))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Json(json!({ "accounts": rows })))
}

#[derive(Deserialize)]
pub struct AccountBody {
    name: String,
    #[serde(flatten)]
    creds: Credentials,
}

/// `{"kind": "hetzner", "name", "token"}` or `{"kind": "aws", "name", "access_key", "secret_key"}`.
pub async fn create_account(State(app): State<AppState>, a: Auth, Json(b): Json<AccountBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let name = b.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("give the account a name"));
    }
    let id = cloud::add_account(&app, name, b.creds).await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    app.audit(Some(&a.0), "cloud.account_add", None, None, Some(name), None);
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_account(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let in_use: bool = app.db.conn().query_row(
        "SELECT (SELECT COUNT(*) FROM cloud_servers WHERE account_id = ?1) + (SELECT COUNT(*) FROM server_pools WHERE account_id = ?1) > 0",
        [&id],
        |r| r.get(0),
    )?;
    if in_use {
        return Err(ApiError::conflict("remove this account's servers and pools first"));
    }
    app.db.conn().execute("DELETE FROM cloud_accounts WHERE id = ?1", [&id])?;
    app.audit(Some(&a.0), "cloud.account_remove", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}

/// Regions and machine sizes (with prices where the provider has them).
pub async fn options(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let acc = cloud::account(&app, &id).map_err(|e| ApiError::not_found(format!("{e:#}")))?;
    Ok(Json(cloud::options(&acc).await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?))
}

pub async fn list_servers(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, account_id, cloud_id, pool_id, name, region, size, node_id, status, error, created_at FROM cloud_servers ORDER BY created_at")?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?, "account_id": r.get::<_, String>(1)?, "cloud_id": r.get::<_, Option<String>>(2)?,
                "pool_id": r.get::<_, Option<String>>(3)?, "name": r.get::<_, String>(4)?, "region": r.get::<_, String>(5)?,
                "size": r.get::<_, String>(6)?, "node_id": r.get::<_, Option<String>>(7)?, "status": r.get::<_, String>(8)?,
                "error": r.get::<_, Option<String>>(9)?, "created_at": r.get::<_, i64>(10)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Json(json!({ "servers": rows })))
}

#[derive(Deserialize)]
pub struct ServerBody {
    account_id: String,
    region: String,
    size: String,
    /// Address the new server reaches the panel at (default: the panel domain or this server's IP).
    panel_url: Option<String>,
}

/// Create a machine that joins this panel by itself within a few minutes.
pub async fn create_server(State(app): State<AppState>, a: Auth, Json(b): Json<ServerBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let url = b.panel_url.unwrap_or_else(|| cloud::panel_url(&app));
    let id = cloud::create_server(&app, &b.account_id, &b.region, &b.size, None, url.trim_end_matches('/')).await.map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    app.audit(Some(&a.0), "cloud.server_create", None, None, Some(&id), Some(&format!("{} in {}", b.size, b.region)));
    Ok(Json(json!({ "id": id })))
}

/// Delete a machine that has not joined (joined ones are removed under Servers).
pub async fn delete_server(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let node: Option<String> = app.db.conn().query_row("SELECT node_id FROM cloud_servers WHERE id = ?1", [&id], |r| r.get(0)).map_err(|_| ApiError::not_found("no such machine"))?;
    if node.is_some() {
        return Err(ApiError::conflict("this machine joined; remove it under Servers, which also deletes it"));
    }
    cloud::destroy(&app, &id).await?;
    app.audit(Some(&a.0), "cloud.server_delete", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn list_pools(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    Ok(Json(json!({ "pools": cloud::pools(&app)? })))
}

#[derive(Deserialize)]
pub struct PoolBody {
    name: Option<String>,
    account_id: Option<String>,
    region: Option<String>,
    size: Option<String>,
    labels: Option<BTreeMap<String, String>>,
    min_servers: Option<u32>,
    max_servers: Option<u32>,
    scale_up_cpu: Option<f64>,
    scale_down_cpu: Option<f64>,
}

fn check_pool(min: u32, max: u32, up: f64, down: f64) -> ApiResult<()> {
    if max < min || max > 100 {
        return Err(ApiError::bad_request("the most servers must be at least the fewest, and at most 100"));
    }
    if !(0.0..=100.0).contains(&down) || !(0.0..=100.0).contains(&up) || down >= up {
        return Err(ApiError::bad_request("the CPU to shrink at must be below the CPU to grow at (both 0 to 100)"));
    }
    Ok(())
}

pub async fn create_pool(State(app): State<AppState>, a: Auth, Json(b): Json<PoolBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let name = b.name.unwrap_or_default().trim().to_lowercase();
    if !util::valid_name(&name) {
        return Err(ApiError::bad_request("pool names are lowercase letters, digits and dashes"));
    }
    let (Some(account), Some(region), Some(size)) = (b.account_id, b.region, b.size) else {
        return Err(ApiError::bad_request("pick the account, region and size"));
    };
    cloud::account(&app, &account).map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
    let (min, max, up, down) = (b.min_servers.unwrap_or(0), b.max_servers.unwrap_or(3), b.scale_up_cpu.unwrap_or(75.0), b.scale_down_cpu.unwrap_or(25.0));
    check_pool(min, max, up, down)?;
    let id = util::id();
    app.db.conn().execute(
        "INSERT INTO server_pools (id, name, account_id, region, size, labels, min_servers, max_servers, scale_up_cpu, scale_down_cpu, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![id, name, account, region, size, serde_json::to_string(&b.labels.unwrap_or_default())?, min, max, up, down, util::now()],
    )?;
    app.audit(Some(&a.0), "cloud.pool_create", None, None, Some(&name), None);
    Ok(Json(json!({ "id": id })))
}

pub async fn update_pool(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Json(b): Json<PoolBody>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let p = cloud::pools(&app)?.into_iter().find(|p| p.id == id).ok_or_else(|| ApiError::not_found("no such pool"))?;
    let (min, max) = (b.min_servers.unwrap_or(p.min_servers), b.max_servers.unwrap_or(p.max_servers));
    let (up, down) = (b.scale_up_cpu.unwrap_or(p.scale_up_cpu), b.scale_down_cpu.unwrap_or(p.scale_down_cpu));
    check_pool(min, max, up, down)?;
    let labels = match b.labels {
        Some(l) => serde_json::to_string(&l)?,
        None => p.labels.to_string(),
    };
    app.db.conn().execute(
        "UPDATE server_pools SET region = ?1, size = ?2, labels = ?3, min_servers = ?4, max_servers = ?5, scale_up_cpu = ?6, scale_down_cpu = ?7 WHERE id = ?8",
        params![b.region.unwrap_or(p.region), b.size.unwrap_or(p.size), labels, min, max, up, down, id],
    )?;
    app.audit(Some(&a.0), "cloud.pool_update", None, None, Some(&p.name), None);
    Ok(Json(json!({ "ok": true })))
}

/// Delete a pool. Its servers stay; set its fewest and most to 0 first to remove them.
pub async fn delete_pool(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    app.db.conn().execute("UPDATE cloud_servers SET pool_id = NULL WHERE pool_id = ?1", [&id])?;
    app.db.conn().execute("DELETE FROM server_pools WHERE id = ?1", [&id])?;
    app.audit(Some(&a.0), "cloud.pool_remove", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}
