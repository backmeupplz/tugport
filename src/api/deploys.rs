//! Deploys, rollbacks, restarts, deploy logs and the live event stream.

use super::service_or_404;
use super::{ApiError, ApiResult, Auth, Events, check_ref, ok, sse_events, sse_json};
use crate::app::{AppState, Event};
use crate::auth::{Role, UserKind};
use crate::deploy::{self, Request};
use crate::runtime;
use crate::store;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::sse;
use futures_util::StreamExt;
use futures_util::future::ready;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct DeployBody {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    /// Block until the deploy finishes and return its outcome and log tail.
    wait: bool,
    timeout_s: Option<u64>,
}

fn trigger_for(a: &Auth) -> &'static str {
    match (a.0.kind, a.0.key_id.is_some()) {
        (UserKind::Agent, _) => "agent",
        (_, true) => "api",
        _ => "manual",
    }
}

pub async fn deploy(State(app): State<AppState>, a: Auth, Path(id): Path<String>, body: Option<Json<DeployBody>>) -> ApiResult<Json<Value>> {
    let b = body.map(|b| b.0).unwrap_or_default();
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    check_ref(b.git_ref.as_deref())?;
    let mut events = app.events.subscribe();
    let req = Request { trigger: trigger_for(&a).into(), actor: Some(a.0.name.clone()), git_ref: b.git_ref, ..Default::default() };
    let did = deploy::enqueue(&app, &s, req)?;
    a.audit_service(&app, "deploy.start", &s, Some(&did));
    if !b.wait {
        return Ok(Json(json!({ "deploy_id": did, "status": "queued" })));
    }
    let timeout = Duration::from_secs(b.timeout_s.unwrap_or(1800).min(7200));
    let _ = tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(e) if e.kind == "deploy" && e.data["deploy_id"] == did.as_str() && deploy::FINAL.contains(&e.data["status"].as_str().unwrap_or("")) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    })
    .await;
    let d = deploy::get(&app, &did)?.ok_or_else(|| ApiError::not_found("deploy vanished"))?;
    let log = deploy::read_log(&app, &did);
    let tail: Vec<String> = log[log.len().saturating_sub(40)..].iter().map(|(_, l)| l.clone()).collect();
    Ok(Json(json!({ "deploy": d, "log_tail": tail })))
}

pub async fn list(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    Ok(Json(json!({ "deploys": deploy::recent(&app, &s.id, 50)?, "live": runtime::live_deploy(&app, &s.id) })))
}

/// The latest deploys across every service the caller can see, for the Actions page.
pub async fn list_all(State(app): State<AppState>, a: Auth) -> ApiResult<Json<Value>> {
    let services: std::collections::HashMap<String, store::Service> = super::visible_services(&app, &a)?.into_iter().map(|s| (s.id.clone(), s)).collect();
    // ponytail: reads the newest 500 and filters; someone who sees few services gets fewer than 100.
    let deploys: Vec<Value> = deploy::recent_all(&app, 500)?
        .into_iter()
        .filter_map(|d| {
            let s = services.get(&d.service_id)?;
            let mut v = serde_json::to_value(&d).ok()?;
            v["service"] = json!(s.name);
            v["project"] = json!(s.project);
            Some(v)
        })
        .take(100)
        .collect();
    Ok(Json(json!({ "deploys": deploys })))
}

fn deploy_or_404(app: &AppState, a: &Auth, id: &str, role: Role) -> ApiResult<(deploy::Deploy, store::Service)> {
    let d = deploy::get(app, id)?.ok_or_else(|| ApiError::not_found("deploy not found"))?;
    let s = service_or_404(app, a, &d.service_id, role)?;
    Ok((d, s))
}

pub async fn get_one(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (d, _) = deploy_or_404(&app, &a, &id, Role::Viewer)?;
    let log: Vec<Value> = deploy::read_log(&app, &id).into_iter().map(|(ts, l)| json!({ "ts": ts, "line": l })).collect();
    Ok(Json(json!({ "deploy": d, "log": log })))
}

/// The deploy log so far, then new lines as they arrive, until the deploy finishes.
pub async fn log_stream(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Events> {
    let (d, _) = deploy_or_404(&app, &a, &id, Role::Viewer)?;
    // Subscribe before reading the file so no line falls in between.
    let rx = app.events.subscribe();
    let line = |ts: &Value, l: &Value| sse_json("line", &json!({ "ts": ts, "line": l }));
    let backlog: Vec<sse::Event> = deploy::read_log(&app, &id).into_iter().map(|(ts, l)| line(&json!(ts), &json!(l))).collect();
    let tail: futures_util::stream::BoxStream<'static, sse::Event> = if deploy::FINAL.contains(&d.status.as_str()) {
        futures_util::stream::iter([sse::Event::default().event("status").data(d.status.clone())]).boxed()
    } else {
        crate::util::subscribe(rx)
            .filter_map(move |e| {
                ready(match e.kind.as_str() {
                    _ if e.data["deploy_id"] != id.as_str() => None,
                    "deploy.log" => Some((line(&e.data["ts"], &e.data["line"]), false)),
                    "deploy" => {
                        let status = e.data["status"].as_str().unwrap_or("");
                        Some((sse::Event::default().event("status").data(status), deploy::FINAL.contains(&status)))
                    }
                    _ => None,
                })
            })
            .scan(false, |finished, (ev, fin)| {
                let out = if *finished { None } else { Some(ev) };
                *finished |= fin;
                ready(out)
            })
            .boxed()
    };
    Ok(sse_events(futures_util::stream::iter(backlog).chain(tail)))
}

pub async fn cancel(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (d, s) = deploy_or_404(&app, &a, &id, Role::Deployer)?;
    let ok = deploy::cancel(&app, &d.id);
    a.audit_service(&app, "deploy.cancel", &s, Some(&d.id));
    Ok(Json(json!({ "cancelled": ok })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RollbackBody {
    deploy_id: Option<String>,
}

pub async fn rollback(State(app): State<AppState>, a: Auth, Path(id): Path<String>, body: Option<Json<RollbackBody>>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    let live = runtime::live_deploy(&app, &s.id);
    let previous = deploy::previous_images(&app, &s.id, 20)?;
    let target = match body.and_then(|b| b.0.deploy_id) {
        Some(want) => previous.into_iter().find(|(d, _)| *d == want).ok_or_else(|| ApiError::bad_request("that deploy has no image to roll back to"))?,
        None => previous.into_iter().find(|(d, _)| Some(d) != live.as_ref()).ok_or_else(|| ApiError::bad_request("there is no earlier version to roll back to"))?,
    };
    let req = Request { trigger: "rollback".into(), actor: Some(a.0.name.clone()), image: Some(target.1.clone()), ..Default::default() };
    let did = deploy::enqueue(&app, &s, req)?;
    a.audit_service(&app, "deploy.rollback", &s, Some(&target.0));
    Ok(Json(json!({ "deploy_id": did, "image": target.1 })))
}

pub async fn restart(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    for r in runtime::current(&app, &s).await? {
        runtime::restart(&app, &r.node, &r.id).await?;
    }
    a.audit_service(&app, "service.restart", &s, None);
    crate::proxy::reload(&app);
    Ok(ok())
}

pub async fn stop(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    for r in runtime::replicas(&app, &s.id).await? {
        runtime::stop(&app, &r.node, &r.id, 30).await?;
    }
    // Nothing starts it on its own again (not even a server coming back) until someone does.
    app.db.set_state(&format!("stopped:{}", s.id), "1")?;
    a.audit_service(&app, "service.stop", &s, None);
    app.emit("service.status", Some(&s.project_id), Some(&s.id), json!({ "state": "stopped" }));
    Ok(ok())
}

pub async fn start(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    let reps = runtime::current(&app, &s).await?;
    if reps.is_empty() {
        return Err(ApiError::bad_request("nothing to start; deploy the service first"));
    }
    for r in reps {
        runtime::start(&app, &r.node, &r.id).await?;
    }
    let _ = app.db.exec("DELETE FROM state WHERE key IN (?1, ?2)", [format!("asleep:{}", s.id), format!("stopped:{}", s.id)]);
    crate::sleep::mark_active(&app, &s.id);
    a.audit_service(&app, "service.start", &s, None);
    crate::proxy::reload(&app);
    Ok(ok())
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct EventsQuery {
    project: Option<String>,
    service: Option<String>,
}

/// Everything happening that the caller may see: deploys, status changes, audit entries.
pub async fn events(State(app): State<AppState>, a: Auth, Query(q): Query<EventsQuery>) -> ApiResult<Events> {
    let p = a.0;
    let stream = crate::util::subscribe(app.events.subscribe())
        .filter(move |e: &Event| {
            let visible = match &e.project_id {
                Some(pid) => p.can(pid, Role::Viewer),
                // Server-wide events (servers, metrics, alerts) go to whoever may see servers; the
                // audit trail outside projects (sign-ins, users, keys) only to the owner.
                None => p.is_owner || (p.all_projects_role.is_some() && e.kind != "audit"),
            };
            let wanted = q.project.as_ref().is_none_or(|x| e.project_id.as_ref() == Some(x)) && q.service.as_ref().is_none_or(|x| e.service_id.as_ref() == Some(x));
            ready(visible && wanted)
        })
        .map(|e| sse_json(&e.kind, &e));
    Ok(sse_events(stream))
}
