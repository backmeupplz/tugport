//! Incoming webhooks: per-service deploy URLs (also Easypanel's `/api/deploy/<token>`)
//! and push events from Git hosts.

use super::{ApiError, ApiResult};
use crate::app::AppState;
use crate::deploy::{self, Request};
use crate::model::Source;
use crate::{git, store, util};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct HookQuery {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
}

pub async fn deploy_hook(State(app): State<AppState>, Path(token): Path<String>, Query(q): Query<HookQuery>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<Value>> {
    let id: Option<String> = app
        .db
        .conn()
        .query_row("SELECT id FROM services WHERE deploy_token_hash = ?1", [util::sha256_hex(token.trim())], |r| r.get(0))
        .optional()?;
    let Some(id) = id else {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        return Err(ApiError::not_found("unknown deploy hook"));
    };
    let s = store::service(&app, &id)?.ok_or_else(|| ApiError::not_found("unknown deploy hook"))?;
    if q.git_ref.as_deref().is_some_and(|r| !git::valid_ref(r)) {
        return Err(ApiError::bad_request("ref is not a branch, tag or commit name"));
    }
    let mut req = Request { trigger: "webhook".into(), actor: Some("deploy hook".into()), git_ref: q.git_ref, ..Default::default() };
    // Easypanel pointed GitHub webhooks at this URL: only pushes to the deployed branch count.
    if headers.contains_key("x-github-event") {
        let push = git::parse_push(&headers, &body);
        let branch = match &s.spec.source {
            Source::Git { branch, .. } => Some(branch),
            _ => None,
        };
        let Some(push) = push.filter(|p| branch.is_some_and(|b| *b == p.branch)) else {
            return Ok(Json(json!({ "ignored": "not a push to the deployed branch" })));
        };
        // The body is not signed, so it only says that the branch moved: the deploy fetches the
        // branch itself rather than a commit named in the body (which could be any commit).
        let actor = (!push.pusher.is_empty()).then_some(push.pusher);
        req = Request { trigger: "push".into(), actor, git_ref: Some(push.branch), ..Default::default() };
    }
    let did = deploy::enqueue(&app, &s, req)?;
    app.audit(None, "deploy.webhook", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&did));
    Ok(Json(json!({ "deploy_id": did })))
}

pub async fn git_hook(State(app): State<AppState>, Path(provider_id): Path<String>, headers: HeaderMap, body: Bytes) -> ApiResult<Json<Value>> {
    let p = git::provider(&app, &provider_id)?.ok_or_else(|| ApiError::not_found("unknown provider"))?;
    if !git::verify_webhook(&p, &headers, &body) {
        return Err(ApiError::unauthorized("the webhook signature does not match"));
    }
    let Some(push) = git::parse_push(&headers, &body) else {
        return Ok(Json(json!({ "ignored": "not a branch push" })));
    };
    let mut started = vec![];
    for s in store::services(&app, None)? {
        let Source::Git { provider: Some(sp), repo, branch, watch_paths, auto_deploy, .. } = &s.spec.source else { continue };
        if sp != &p.id || !repo.eq_ignore_ascii_case(&push.repo) || branch != &push.branch || !auto_deploy {
            continue;
        }
        if push.message.contains("[skip deploy]") || push.message.contains("[skip ci]") {
            continue;
        }
        if !git::push_touches(watch_paths, push.changed.as_deref()) {
            continue;
        }
        let req = Request {
            trigger: "push".into(),
            actor: (!push.pusher.is_empty()).then(|| push.pusher.clone()),
            git_ref: Some(push.branch.clone()),
            commit_sha: Some(push.sha.clone()),
            commit_message: Some(push.message.clone()),
            ..Default::default()
        };
        let did = deploy::enqueue(&app, &s, req)?;
        started.push(json!({ "service": format!("{}.{}", s.project, s.name), "deploy_id": did }));
    }
    Ok(Json(json!({ "deploys": started })))
}
