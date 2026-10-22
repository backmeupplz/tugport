//! Deploys, rollbacks, restarts, deploy logs and the live event stream.

use super::projects::service_or_404;
use super::{ApiError, ApiResult, Auth};
use crate::app::{AppState, Event};
use crate::auth::{Role, UserKind};
use crate::deploy::{self, Request};
use crate::runtime;
use crate::store;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::sse::{self, KeepAlive, Sse};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
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
    let mut events = app.events.subscribe();
    let req = Request { trigger: trigger_for(&a).into(), actor: Some(a.0.name.clone()), git_ref: b.git_ref, ..Default::default() };
    let did = deploy::enqueue(&app, &s, req)?;
    app.audit(Some(&a.0), "deploy.start", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&did));
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
    let tail: Vec<String> = deploy::read_log(&app, &did).into_iter().rev().take(40).map(|(_, l)| l).collect::<Vec<_>>().into_iter().rev().collect();
    Ok(Json(json!({ "deploy": d, "log_tail": tail })))
}

pub async fn list(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Viewer)?;
    Ok(Json(json!({ "deploys": deploy::recent(&app, &s.id, 50)?, "live": runtime::live_deploy(&app, &s.id) })))
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
pub async fn log_stream(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>> {
    let (d, _) = deploy_or_404(&app, &a, &id, Role::Viewer)?;
    // Subscribe before reading the file so no line falls in between.
    let rx = app.events.subscribe();
    let line = |ts: &Value, l: &Value| sse::Event::default().event("line").data(json!({ "ts": ts, "line": l }).to_string());
    let backlog: Vec<sse::Event> = deploy::read_log(&app, &id).into_iter().map(|(ts, l)| line(&json!(ts), &json!(l))).collect();
    let tail: futures_util::stream::BoxStream<'static, sse::Event> = if deploy::FINAL.contains(&d.status.as_str()) {
        futures_util::stream::iter([sse::Event::default().event("status").data(d.status.clone())]).boxed()
    } else {
        let did = id.clone();
        tokio_stream::wrappers::BroadcastStream::new(rx)
            .filter_map(move |e| {
                let did = did.clone();
                async move {
                    let e = e.ok()?;
                    if e.data["deploy_id"] != did.as_str() {
                        return None;
                    }
                    match e.kind.as_str() {
                        "deploy.log" => Some((line(&e.data["ts"], &e.data["line"]), false)),
                        "deploy" => {
                            let status = e.data["status"].as_str().unwrap_or("").to_string();
                            let fin = deploy::FINAL.contains(&status.as_str());
                            Some((sse::Event::default().event("status").data(status), fin))
                        }
                        _ => None,
                    }
                }
            })
            .scan(false, |finished, (ev, fin)| {
                let out = if *finished { None } else { Some(ev) };
                *finished |= fin;
                futures_util::future::ready(out)
            })
            .boxed()
    };
    let stream = futures_util::stream::iter(backlog).chain(tail).map(Ok);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn cancel(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (d, s) = deploy_or_404(&app, &a, &id, Role::Deployer)?;
    let ok = deploy::cancel(&app, &d.id);
    app.audit(Some(&a.0), "deploy.cancel", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&d.id));
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
    app.audit(Some(&a.0), "deploy.rollback", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&target.0));
    Ok(Json(json!({ "deploy_id": did, "image": target.1 })))
}

pub async fn restart(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    let live = runtime::live_deploy(&app, &s.id);
    for r in runtime::replicas(&app, &s.id).await? {
        if s.kind == "compose" || live.as_deref() == Some(r.deploy.as_str()) {
            runtime::restart(&app, &r.node, &r.id).await?;
        }
    }
    app.audit(Some(&a.0), "service.restart", Some(&s.project_id), Some(&s.id), Some(&s.name), None);
    crate::proxy::reload(&app);
    Ok(Json(json!({ "ok": true })))
}

pub async fn stop(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    for r in runtime::replicas(&app, &s.id).await? {
        runtime::stop(&app, &r.node, &r.id, 30).await?;
    }
    // Nothing starts it on its own again (not even a server coming back) until someone does.
    app.db.set_state(&format!("stopped:{}", s.id), "1")?;
    app.audit(Some(&a.0), "service.stop", Some(&s.project_id), Some(&s.id), Some(&s.name), None);
    app.emit("service.status", Some(&s.project_id), Some(&s.id), json!({ "state": "stopped" }));
    Ok(Json(json!({ "ok": true })))
}

pub async fn start(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    let live = runtime::live_deploy(&app, &s.id);
    let reps: Vec<_> = runtime::replicas(&app, &s.id).await?.into_iter().filter(|r| s.kind == "compose" || live.as_deref() == Some(r.deploy.as_str())).collect();
    if reps.is_empty() {
        return Err(ApiError::bad_request("nothing to start; deploy the service first"));
    }
    for r in reps {
        runtime::start(&app, &r.node, &r.id).await?;
    }
    let _ = app.db.conn().execute("DELETE FROM state WHERE key IN (?1, ?2)", [format!("asleep:{}", s.id), format!("stopped:{}", s.id)]);
    app.audit(Some(&a.0), "service.start", Some(&s.project_id), Some(&s.id), Some(&s.name), None);
    crate::proxy::reload(&app);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct EventsQuery {
    project: Option<String>,
    service: Option<String>,
}

/// Everything happening that the caller may see: deploys, status changes, audit entries.
pub async fn events(State(app): State<AppState>, a: Auth, Query(q): Query<EventsQuery>) -> ApiResult<Sse<impl Stream<Item = Result<sse::Event, Infallible>>>> {
    let p = a.0.clone();
    let rx = app.events.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |e: Result<Event, _>| {
        let p = p.clone();
        let q_project = q.project.clone();
        let q_service = q.service.clone();
        async move {
            let e = e.ok()?;
            let visible = match &e.project_id {
                Some(pid) => p.can(pid, Role::Viewer),
                None => p.is_owner,
            };
            let wanted = q_project.as_ref().is_none_or(|x| e.project_id.as_ref() == Some(x)) && q_service.as_ref().is_none_or(|x| e.service_id.as_ref() == Some(x));
            (visible && wanted).then(|| Ok(sse::Event::default().event(e.kind.clone()).data(serde_json::to_string(&e).unwrap_or_default())))
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
