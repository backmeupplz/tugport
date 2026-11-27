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
    /// Build without the layer cache and pull base images again.
    no_cache: bool,
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
    // This process execs before the deploy starts, which would cut off the wait.
    if b.wait && crate::handoff::draining() {
        return Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "Tugport is restarting; try again in a few seconds"));
    }
    let mut events = app.events.subscribe();
    let req = Request { trigger: trigger_for(&a).into(), actor: Some(a.0.name.clone()), git_ref: b.git_ref, no_cache: b.no_cache, ..Default::default() };
    let did = deploy::enqueue(&app, &s, req)?;
    a.audit_service(&app, "deploy.start", &s, Some(&did));
    if !b.wait {
        return Ok(Json(json!({ "deploy_id": did, "status": "queued" })));
    }
    let timeout = Duration::from_secs(b.timeout_s.unwrap_or(1800).min(7200));
    let finished = tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(e) if e.kind == "deploy" && e.data["deploy_id"] == did.as_str() && deploy::FINAL.contains(&e.data["status"].as_str().unwrap_or("")) => break,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    });
    // A reload that starts meanwhile hands the deploy to the next process: answer with where it stands.
    tokio::select! {
        _ = finished => {}
        _ = crate::handoff::drained() => {}
    }
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
    start_or_restart(app, a, id, true).await
}

pub async fn stop(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    // Nothing starts it on its own again (not even a server coming back or a request to a
    // sleeping service) until someone does. Set first: a wake-up in progress then gives way
    // within a moment, and the replicas are listed only once it has, so none starts after.
    app.db.set_state(&format!("stopped:{}", s.id), "1")?;
    // Carried through even if the caller goes away meanwhile, which would leave it marked
    // stopped and running.
    tokio::spawn(async move {
        let gate = crate::sleep::turn(&s.id);
        let mut deadline = gate.lock().await;
        // Again: a start or restart that had the turn first cleared it.
        app.db.set_state(&format!("stopped:{}", s.id), "1")?;
        // Every replica, even when one fails: as few as possible keep running.
        let mut failed = None;
        match runtime::replicas(&app, &s.id).await {
            Ok(reps) => {
                for r in reps {
                    if let Err(e) = runtime::stop(&app, &r.node, &r.id, 30).await {
                        tracing::warn!("stopping {}: {e:#}", r.name);
                        failed.get_or_insert(e);
                    }
                }
            }
            Err(e) => failed = Some(e),
        }
        app.db.del_state(&format!("asleep:{}", s.id))?;
        *deadline = None;
        // An asleep service's containers were already stopped, so no Docker event rebuilds its route.
        crate::proxy::reload(&app);
        if let Some(e) = failed {
            return Err(e.into());
        }
        a.audit_service(&app, "service.stop", &s, None);
        app.emit("service.status", Some(&s.project_id), Some(&s.id), json!({ "state": "stopped" }));
        Ok(ok())
    })
    .await
    .map_err(|e| ApiError::from(anyhow::anyhow!("stopping: {e}")))?
}

pub async fn start(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    start_or_restart(app, a, id, false).await
}

/// Carried through even if the caller goes away meanwhile (a sleeping service's wake-up can
/// take a while), which would leave it half started.
async fn start_or_restart(app: AppState, a: Auth, id: String, restart: bool) -> ApiResult<Json<Value>> {
    tokio::spawn(start_or_restart_now(app, a, id, restart)).await.map_err(|e| ApiError::from(anyhow::anyhow!("starting: {e}")))?
}

async fn start_or_restart_now(app: AppState, a: Auth, id: String, restart: bool) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Deployer)?;
    // Not while a wake-up or sleep_idle acts on it, which could stop it again.
    let gate = crate::sleep::turn(&s.id);
    let mut deadline = gate.lock().await;
    let reps = runtime::current(&app, &s).await?;
    if reps.is_empty() {
        return Err(ApiError::bad_request("nothing to run; deploy the service first"));
    }
    // A sleeping service's routes stay without targets meanwhile: requests wait for the wake-up.
    for r in reps {
        // Docker's restart starts stopped containers too.
        if restart { runtime::restart(&app, &r.node, &r.id).await? } else { runtime::start(&app, &r.node, &r.id).await? }
    }
    // Running: nothing keeps it down any more, not even a server coming back.
    app.db.del_state(&format!("stopped:{}", s.id))?;
    if !wake_asleep(&app, &s, &mut deadline).await? {
        app.db.del_state(&format!("asleep:{}", s.id))?;
    }
    crate::sleep::mark_active(&app, &s.id);
    a.audit_service(&app, if restart { "service.restart" } else { "service.start" }, &s, None);
    crate::proxy::reload(&app);
    Ok(ok())
}

/// Start or restart of a sleeping service (with a domain): wait for it to answer as a wake-up
/// would, so requests reach it only then. Returns whether it did. On failure it stays asleep
/// and sleep_idle stops what started, as after a request's failed wake-up.
async fn wake_asleep(app: &AppState, s: &crate::store::Service, deadline: &mut Option<std::time::Instant>) -> ApiResult<bool> {
    let Some(d) = crate::store::domains(app, Some(&s.id))?.into_iter().next().filter(|_| crate::sleep::is_asleep(app, &s.id)) else {
        return Ok(false);
    };
    // Asked for: not cut short by an earlier wake-up that gave up.
    *deadline = None;
    match crate::sleep::wake_in_turn(app, &s.id, d.port, deadline).await {
        Ok(_) => Ok(true),
        // A stop came in meanwhile and goes ahead once this gives way.
        Err(e) if app.db.state::<String>(&format!("stopped:{}", s.id)).is_some() => Err(ApiError::conflict(format!("{e:#}"))),
        Err(e) => Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}"))),
    }
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
                // audit trail outside projects (sign-ins, users, keys) and the log and progress of
                // Tugport's own backups only to the owner.
                None => p.is_owner || (p.all_projects_role.is_some() && e.kind != "audit" && !e.kind.starts_with("backup.")),
            };
            let wanted = q.project.as_ref().is_none_or(|x| e.project_id.as_ref() == Some(x)) && q.service.as_ref().is_none_or(|x| e.service_id.as_ref() == Some(x));
            ready(visible && wanted)
        })
        .map(|e| sse_json(&e.kind, &e));
    Ok(sse_events(stream))
}
