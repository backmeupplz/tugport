//! Connecting Git hosts: one-click GitHub App, tokens for GitHub/Forgejo/GitLab, deploy keys.

use super::{ApiError, ApiResult, Auth, Owner, base_url, ok, service_or_404};
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

/// What a token for `kind` must allow: listing repositories, managing their webhooks and
/// posting commit statuses.
fn needs(kind: Kind) -> &'static str {
    match kind {
        Kind::Forgejo => "Give it access to all repositories with repository: Read and write, user: Read and organization: Read.",
        Kind::Gitlab => "Give it the api scope.",
        _ => "Give it the repo and admin:repo_hook scopes.",
    }
}

/// Connect a host with an access token (Forgejo/Gitea, GitLab, or a GitHub token).
pub async fn create(State(app): State<AppState>, Owner(a): Owner, Json(b): Json<TokenProvider>) -> ApiResult<Json<Value>> {
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
    let p = git::provider(&app, &id)?.ok_or_else(|| ApiError::not_found("the provider was not saved"))?;
    if let Err(e) = git::repos(&p).await {
        app.db.exec("DELETE FROM git_providers WHERE id = ?1", [&id])?;
        return Err(ApiError::bad_request(format!("the token does not work ({e:#}). {}", needs(kind))));
    }
    a.audit(&app, "git.connect", Some(&name), Some(kind.as_str()));
    Ok(Json(json!({ "id": id, "name": name })))
}

#[derive(Deserialize)]
pub struct TokenBody {
    token: String,
}

/// Swap a connection's token (a rotation) after checking the new one works. The connection
/// keeps its id and webhook secret, so services and webhooks stay as they are.
pub async fn replace_token(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>, Json(b): Json<TokenBody>) -> ApiResult<Json<Value>> {
    let old = provider_or_404(&app, &id)?;
    if old.kind == Kind::GithubApp {
        return Err(ApiError::bad_request("a GitHub App connection gets fresh tokens by itself"));
    }
    let mut p = old.clone();
    p.config["token"] = json!(b.token.trim());
    if let Err(e) = git::repos(&p).await {
        return Err(ApiError::bad_request(format!("the token does not work ({e:#}). {}", needs(p.kind))));
    }
    git::save_provider(&app, Some(&id), p.kind, &p.name, &p.base_url, &p.config)?;
    a.audit(&app, "git.token_replaced", Some(&p.name), Some(p.kind.as_str()));
    Ok(ok())
}

pub async fn delete(State(app): State<AppState>, Owner(a): Owner, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let used = crate::store::services(&app, None)?.into_iter().filter(|s| matches!(&s.spec.source, Source::Git { provider: Some(p), .. } if *p == id)).count();
    if used > 0 {
        return Err(ApiError::conflict(format!(
            "{used} {} still deploy from this connection; to rotate its token, use Replace token instead",
            if used == 1 { "service" } else { "services" }
        )));
    }
    app.db.exec("DELETE FROM git_providers WHERE id = ?1", [&id])?;
    a.audit(&app, "git.disconnect", Some(&id), None);
    Ok(ok())
}

pub async fn repos(State(app): State<AppState>, a: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    // The list includes the owner's private repositories: only for people who set up services.
    let p = &a.0;
    if !p.is_owner && p.all_projects_role.max(p.project_roles.values().copied().max()).is_none_or(|r| r < Role::Operator) {
        return Err(ApiError::forbidden("listing repositories needs the operator role on a project"));
    }
    Ok(Json(json!({ "repos": git::repos(&provider_or_404(&app, &id)?).await? })))
}

pub(super) fn provider_or_404(app: &AppState, id: &str) -> ApiResult<git::Provider> {
    git::provider(app, id)?.ok_or_else(|| ApiError::not_found("unknown provider"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ManifestBody {
    organization: Option<String>,
}

/// Start creating a GitHub App. The browser posts `manifest` to `action`; GitHub then
/// redirects back to the callback below.
pub async fn github_manifest(State(app): State<AppState>, _: Owner, headers: HeaderMap, body: Option<Json<ManifestBody>>) -> ApiResult<Json<Value>> {
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
        // After installing on repositories, GitHub sends people back to the panel.
        "setup_url": format!("{base}/settings/git"),
        "setup_on_update": true,
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

pub async fn github_callback(State(app): State<AppState>, Owner(a): Owner, Query(q): Query<Callback>) -> ApiResult<impl IntoResponse> {
    let key = format!("gh_manifest:{}", q.state);
    let id = app.db.get_state(&key)?.ok_or_else(|| ApiError::bad_request("this GitHub App setup link expired; start again"))?;
    app.db.del_state(&key)?;
    let (slug, install) = git::finish_github_manifest(&app, &id, &q.code).await?;
    a.audit(&app, "git.connect", Some(&slug), Some("github_app"));
    Ok(Redirect::to(&install))
}

/// The address GitHub and other hosts can reach us on.
fn public_base(app: &AppState, headers: &HeaderMap) -> String {
    match app.settings().panel_domain {
        Some(d) => format!("https://{d}"),
        None => base_url(headers),
    }
}

/// In the background, make sure the service's repository sends pushes to us (providers without
/// app-level webhooks).
pub fn ensure_hook_soon(app: &AppState, headers: &HeaderMap, service_id: &str) -> ApiResult<()> {
    let Some(s) = crate::store::service(app, service_id)? else { return Ok(()) };
    let Source::Git { provider: Some(pid), repo, auto_deploy: true, .. } = &s.spec.source else { return Ok(()) };
    let Ok(Some(p)) = git::provider(app, pid) else { return Ok(()) };
    let (app, url, repo) = (app.clone(), format!("{}/api/hooks/git/{}", public_base(app, headers), p.id), repo.clone());
    tokio::spawn(async move {
        if let Err(e) = git::ensure_webhook(&p, &repo, &url).await {
            tracing::warn!("could not create the push webhook on {repo}: {e:#}");
            app.emit("warning", Some(&s.project_id), Some(&s.id), json!({ "message": format!("could not create the push webhook: {e:#}") }));
        }
    });
    Ok(())
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
    let deploy_key = format!("deploykey:{}", s.id);
    app.set_sealed_state(&deploy_key, &deploy_key, &private)?;
    a.audit_service(&app, "service.deploy_key", &s, None);
    Ok(Json(json!({ "public_key": public.trim() })))
}
