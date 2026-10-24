//! Database read replicas: add, list, promote.

use super::service_or_404;
use super::{bad, detached};
use super::{ApiError, ApiResult, Auth};
use crate::app::AppState;
use crate::auth::Role;
use crate::model::Placement;
use crate::replicas;
use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ReplicaBody {
    /// Where it runs: a server, a region or labels (any server when left out).
    placement: Placement,
}

/// Add a read replica. It is a database service of its own; it starts following the
/// primary once its first deploy is live (MongoDB and MariaDB primaries restart once to
/// enable replication).
pub async fn add(State(app): State<AppState>, a: Auth, Path(id): Path<String>, body: Option<Json<ReplicaBody>>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if s.kind != "database" {
        return Err(ApiError::bad_request("only databases have replicas"));
    }
    let placement = body.map(|b| b.0.placement).unwrap_or_default();
    let (app2, s2) = (app.clone(), s.clone());
    // Finishes even if the caller goes away (it may restart the primary).
    let (replica, deploy_id) = detached(async move { replicas::add(&app2, &s2, placement).await }).await?.map_err(bad)?;
    app.audit(Some(&a.0), "database.replica_add", Some(&s.project_id), Some(&s.id), Some(&replica.name), None);
    Ok(Json(json!({ "replica": { "id": replica.id, "name": replica.name }, "deploy_id": deploy_id })))
}

pub async fn list(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    let mut out = vec![];
    for r in replicas::of(&app, &s.id)? {
        let status = crate::runtime::service_status(&app, &r).await;
        let nodes: Vec<Value> = status["replicas"].as_array().into_iter().flatten().map(|c| c["node"].clone()).collect();
        out.push(json!({ "id": r.id, "name": r.name, "state": status["state"], "nodes": nodes, "replication": replicas::state(&app, &r.id) }));
    }
    Ok(Json(json!({ "replicas": out })))
}

/// Make a replica a primary of its own: the old primary's URL variable then points at
/// it. Use it when the primary is lost; both would take writes if the old one comes back.
pub async fn promote(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Admin)?;
    let (app2, s2) = (app.clone(), s.clone());
    let var = detached(async move { replicas::promote(&app2, &s2).await }).await?.map_err(bad)?;
    a.audit_service(&app, "database.promote", &s, Some(&var));
    Ok(Json(json!({
        "ok": true,
        "variable": var,
        "message": format!("{} is now a primary and {var} points at it. Services read it on their next deploy or restart.", s.name),
    })))
}
