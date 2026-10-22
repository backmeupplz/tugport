//! Connecting Git hosts: one-click GitHub App, tokens for GitHub/Forgejo/GitLab, deploy keys.

use super::projects::service_or_404;
use super::{ApiError, ApiResult, Auth, base_url};
use crate::app::AppState;
use crate::auth::Role;
use crate::git::{self, Kind};
use crate::model::Source;
use crate::util;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;
use serde_json::{Value, json};

pub async fn list(State(app): State<AppState>, _a: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "providers": git::providers(&app)? })))
}

#[derive(Deserialize)]
pub struct TokenProvider {
    kind: String,
    name: Option<String>,
    base_url: Option<String>,
    token: String,
}

/// Connect a host with an access token (Forgejo/Gitea, GitLab, or a GitHub token).
pub async fn create(State(app): State<AppState>, a: Auth, Json(b): Json<TokenProvider>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let kind = Kind::parse(&b.kind).filter(|k| *k != Kind::GithubApp).ok_or_else(|| ApiError::bad_request("kind is forgejo, gitea, gitlab or github_token"))?;
    let base = b
        .base_url
        .or_else(|| match kind {
            Kind::GithubToken => Some("https://github.com".into()),
            Kind::Gitlab => Some("https://gitlab.com".into()),
            _ => None,
        })
        .ok_or_else(|| ApiError::bad_request("base_url is the address of your Git server, like https://git.example.com"))?;
    let name = b.name.unwrap_or_else(|| base.trim_start_matches("https://").trim_end_matches('/').to_string());
    let config = json!({ "token": b.token.trim(), "webhook_secret": util::token(40) });
    let id = git::save_provider(&app, None, kind, &name, &base, &config)?;
    let p = git::provider(&app, &id)?.unwrap();
    if let Err(e) = git::repos(&p).await {
        app.db.conn().execute("DELETE FROM git_providers WHERE id = ?1", [&id])?;
        return Err(ApiError::bad_request(format!("the token does not work: {e:#}")));
    }
    app.audit(Some(&a.0), "git.connect", None, None, Some(&name), Some(kind.as_str()));
    Ok(Json(json!({ "id": id, "name": name })))
}

pub async fn delete(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let used = crate::store::services(&app, None)?.into_iter().any(|s| matches!(&s.spec.source, Source::Git { provider: Some(p), .. } if *p == id));
    if used {
        return Err(ApiError::conflict("services still deploy from this provider"));
    }
    app.db.conn().execute("DELETE FROM git_providers WHERE id = ?1", [&id])?;
    app.audit(Some(&a.0), "git.disconnect", None, None, Some(&id), None);
    Ok(Json(json!({ "ok": true })))
}

pub async fn repos(State(app): State<AppState>, _a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = git::provider(&app, &id)?.ok_or_else(|| ApiError::not_found("unknown provider"))?;
    Ok(Json(json!({ "repos": git::repos(&p).await? })))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ManifestBody {
    organization: Option<String>,
}

/// Start creating a GitHub App. The browser posts `manifest` to `action`; GitHub then
/// redirects back to the callback below.
pub async fn github_manifest(State(app): State<AppState>, a: Auth, headers: HeaderMap, body: Option<Json<ManifestBody>>) -> ApiResult<Json<Value>> {
    a.require_owner()?;
    let base = public_base(&app, &headers);
    let id = util::id();
    let state = util::token(24);
    app.db.set_state(&format!("gh_manifest:{state}"), &id)?;
    let host = base.trim_start_matches("https://").trim_start_matches("http://");
    let manifest = json!({
        "name": format!("Tugport {}", host.split(':').next().unwrap_or(host)).chars().take(34).collect::<String>(),
        "url": base,
        "hook_attributes": { "url": format!("{base}/api/hooks/git/{id}"), "active": true },
        "redirect_url": format!("{base}/api/git/github/callback"),
        "public": false,
        "default_permissions": { "contents": "read", "metadata": "read", "statuses": "write", "pull_requests": "read" },
        "default_events": ["push"],
    });
    let action = match body.and_then(|b| b.0.organization) {
        Some(org) => format!("https://github.com/organizations/{org}/settings/apps/new?state={state}"),
        None => format!("https://github.com/settings/apps/new?state={state}"),
    };
    Ok(Json(json!({ "action": action, "manifest": manifest })))
}

#[derive(Deserialize)]
pub struct Callback {
    code: String,
    state: String,
}

pub async fn github_callback(State(app): State<AppState>, a: Auth, Query(q): Query<Callback>) -> ApiResult<impl IntoResponse> {
    a.require_owner()?;
    let key = format!("gh_manifest:{}", q.state);
    let id = app.db.get_state(&key)?.ok_or_else(|| ApiError::bad_request("this GitHub App setup link expired; start again"))?;
    app.db.conn().execute("DELETE FROM state WHERE key = ?1", [&key])?;
    let (slug, install) = git::finish_github_manifest(&app, &id, &q.code).await?;
    app.audit(Some(&a.0), "git.connect", None, None, Some(&slug), Some("github_app"));
    Ok(Redirect::to(&install))
}

/// The address GitHub and other hosts can reach us on.
pub fn public_base(app: &AppState, headers: &HeaderMap) -> String {
    match app.settings().panel_domain {
        Some(d) => format!("https://{d}"),
        None => base_url(headers),
    }
}

/// Make sure the repository sends pushes to us (providers without app-level webhooks).
pub async fn ensure_hook(app: &AppState, s: &crate::store::Service, base: &str) {
    let Source::Git { provider: Some(pid), repo, auto_deploy: true, .. } = &s.spec.source else { return };
    let Ok(Some(p)) = git::provider(app, pid) else { return };
    if let Err(e) = git::ensure_webhook(&p, repo, &format!("{base}/api/hooks/git/{}", p.id)).await {
        tracing::warn!("could not create the push webhook on {repo}: {e:#}");
        app.emit("warning", Some(&s.project_id), Some(&s.id), json!({ "message": format!("could not create the push webhook: {e:#}") }));
    }
}

/// Generate an SSH deploy key for a service; returns the public half to add to the repository.
pub async fn deploy_key(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    let dir = app.paths.run.join(format!("keygen-{}", util::id()));
    std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
    let key = dir.join("key");
    let mut cmd = tokio::process::Command::new("ssh-keygen");
    cmd.args(["-q", "-t", "ed25519", "-N", "", "-C"]).arg(format!("tugport-{}.{}", s.project, s.name)).arg("-f").arg(&key);
    let res = crate::proc::output(cmd).await;
    let out = res.and_then(|_| Ok((std::fs::read_to_string(&key)?, std::fs::read_to_string(dir.join("key.pub"))?)));
    let _ = std::fs::remove_dir_all(&dir);
    let (private, public) = out?;
    let sealed = app.keys.seal_str(&format!("deploykey:{}", s.id), &private);
    app.db.set_state(&format!("deploykey:{}", s.id), &crate::crypto::b64(&sealed))?;
    app.audit(Some(&a.0), "service.deploy_key", Some(&s.project_id), Some(&s.id), Some(&s.name), None);
    Ok(Json(json!({ "public_key": public.trim() })))
}
