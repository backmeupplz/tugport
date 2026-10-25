//! Git hosting: providers (GitHub App, GitHub token, Forgejo/Gitea, GitLab), clones,
//! commit statuses, and push webhooks.

use crate::app::App;
use crate::deploy::Deploy;
use crate::model::Source;
use crate::store::Service;
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, RequestBuilder};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    GithubApp,
    GithubToken,
    Forgejo,
    Gitlab,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "github_app" => Kind::GithubApp,
            "github_token" => Kind::GithubToken,
            "forgejo" | "gitea" => Kind::Forgejo,
            "gitlab" => Kind::Gitlab,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::GithubApp => "github_app",
            Kind::GithubToken => "github_token",
            Kind::Forgejo => "forgejo",
            Kind::Gitlab => "gitlab",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    /// Web URL (https://github.com, https://git.example.com).
    pub base_url: String,
    pub config: Value,
}

impl Provider {
    fn secret(&self) -> Option<&str> {
        self.config.get("webhook_secret").and_then(|v| v.as_str())
    }
    fn token(&self) -> Option<&str> {
        self.config.get("token").and_then(|v| v.as_str())
    }
    fn api(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.kind {
            Kind::GithubApp | Kind::GithubToken if base == "https://github.com" => "https://api.github.com".into(),
            Kind::GithubApp | Kind::GithubToken => format!("{base}/api/v3"),
            Kind::Forgejo => format!("{base}/api/v1"),
            Kind::Gitlab => format!("{base}/api/v4"),
        }
    }
    fn clone_url(&self, repo: &str) -> String {
        format!("{}/{}.git", self.base_url.trim_end_matches('/'), repo.trim_end_matches(".git"))
    }
}

pub fn provider(app: &App, id: &str) -> Result<Option<Provider>> {
    let row = app.db.row("SELECT id, kind, name, base_url, config FROM git_providers WHERE id = ?1", [id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get::<_, Vec<u8>>(4)?))
    })?;
    let Some((id, kind, name, base_url, config)) = row else { return Ok(None) };
    let config = serde_json::from_str(&app.keys.open_str(&format!("git:{id}"), &config)?)?;
    Ok(Some(Provider { id, kind: Kind::parse(&kind).ok_or_else(|| anyhow!("unknown provider kind {kind}"))?, name, base_url, config }))
}

pub fn providers(app: &App) -> Result<Vec<Value>> {
    Ok(app.db.rows("SELECT id, kind, name, base_url, created_at FROM git_providers ORDER BY created_at", [], |r| {
        Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?,
                   "base_url": r.get::<_, String>(3)?, "created_at": r.get::<_, i64>(4)? }))
    })?)
}

pub fn save_provider(app: &App, id: Option<&str>, kind: Kind, name: &str, base_url: &str, config: &Value) -> Result<String> {
    let id = id.map(String::from).unwrap_or_else(util::id);
    let sealed = app.keys.seal_str(&format!("git:{id}"), &config.to_string())?;
    app.db.exec(
        "INSERT INTO git_providers (id, kind, name, base_url, config, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, base_url = excluded.base_url, config = excluded.config",
        params![id, kind.as_str(), name, base_url.trim_end_matches('/'), sealed, util::now()],
    )?;
    Ok(id)
}

fn http() -> Result<reqwest::Client> {
    Ok(util::http(30).build()?)
}

/// A request to the provider's API (`path` after its base), authenticated the way its kind expects.
fn api(p: &Provider, method: Method, path: &str, token: &str) -> Result<RequestBuilder> {
    let req = http()?.request(method, format!("{}{path}", p.api()));
    Ok(match p.kind {
        Kind::GithubApp | Kind::GithubToken => req.bearer_auth(token).header("Accept", "application/vnd.github+json"),
        Kind::Forgejo => req.header("Authorization", format!("token {token}")),
        Kind::Gitlab => req.header("PRIVATE-TOKEN", token),
    })
}

async fn send(req: RequestBuilder) -> Result<reqwest::Response> {
    Ok(req.send().await?.error_for_status()?)
}

/// The token that reaches `repo`: the provider's own, or a GitHub App installation token.
async fn repo_token(p: &Provider, repo: &str) -> Result<String> {
    match p.kind {
        Kind::GithubApp => installation_token(p, repo).await,
        _ => Ok(p.token().unwrap_or_default().to_string()),
    }
}

/// Installation tokens by provider and repository owner, with their expiry.
static TOKENS: LazyLock<Mutex<HashMap<String, (String, i64)>>> = LazyLock::new(Default::default);

fn app_jwt(p: &Provider) -> Result<String> {
    let app_id = p.config.get("app_id").and_then(|v| v.as_i64()).ok_or_else(|| anyhow!("GitHub App id missing"))?;
    let pem = p.config.get("pem").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("GitHub App key missing"))?;
    let now = util::now();
    let claims = json!({ "iat": now - 60, "exp": now + 540, "iss": app_id.to_string() });
    crate::crypto::jwt_rs256(pem, &claims)
}

/// Short-lived installation token for a repository the GitHub App can see.
async fn installation_token(p: &Provider, repo: &str) -> Result<String> {
    let owner = repo.split('/').next().unwrap_or(repo).to_lowercase();
    let cache_key = format!("{}:{owner}", p.id);
    if let Some((t, exp)) = util::lock(&TOKENS).get(&cache_key)
        && *exp > util::now() + 300
    {
        return Ok(t.clone());
    }
    let jwt = app_jwt(p)?;
    let inst: Value = send(api(p, Method::GET, &format!("/repos/{repo}/installation"), &jwt)?)
        .await
        .with_context(|| format!("the GitHub App is not installed on {repo}"))?
        .json()
        .await?;
    let inst_id = inst["id"].as_i64().ok_or_else(|| anyhow!("no installation id"))?;
    let (token, exp) = installation_access(p, &jwt, inst_id).await?;
    util::lock(&TOKENS).insert(cache_key, (token.clone(), exp));
    Ok(token)
}

/// A token for one installation of the GitHub App, and when it expires.
async fn installation_access(p: &Provider, jwt: &str, inst_id: i64) -> Result<(String, i64)> {
    let tok: Value = send(api(p, Method::POST, &format!("/app/installations/{inst_id}/access_tokens"), jwt)?).await?.json().await?;
    let token = tok["token"].as_str().ok_or_else(|| anyhow!("no installation token"))?.to_string();
    let exp = chrono::DateTime::parse_from_rfc3339(tok["expires_at"].as_str().unwrap_or("")).map(|d| d.timestamp()).unwrap_or(util::now() + 3000);
    Ok((token, exp))
}

/// HTTP Basic credentials for cloning, passed per command so they never land in .git/config.
async fn clone_header(p: &Provider, repo: &str) -> Result<String> {
    let user = match p.kind {
        Kind::GithubApp | Kind::GithubToken => "x-access-token",
        Kind::Forgejo => "tugport",
        Kind::Gitlab => "oauth2",
    };
    let pass = repo_token(p, repo).await?;
    Ok(format!("Authorization: Basic {}", crate::crypto::b64(format!("{user}:{pass}").as_bytes())))
}

/// GIT_SSH_COMMAND for the service's deploy key, if it has one: the key is written to `key_path`,
/// which the caller removes once git is done.
fn deploy_key_ssh(app: &App, s: &Service, key_path: &Path) -> Result<Option<String>> {
    let name = format!("deploykey:{}", s.id);
    let Some(mut key) = app.sealed_state(&name, &name)? else { return Ok(None) };
    if !key.ends_with('\n') {
        key.push('\n');
    }
    if let Some(dir) = key_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    util::write_private(key_path, key.as_bytes())?;
    let known = app.paths.data.join("known_hosts");
    Ok(Some(format!("ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}", key_path.display(), known.display())))
}

/// A branch, tag or commit name as git accepts it, and never something git would take as an option.
pub fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && !r.starts_with(['-', '+', '/', '.'])
        && !r.ends_with(['/', '.'])
        && !r.contains("..")
        && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+@#".contains(&b))
}

/// A repository Tugport may fetch: a provider's `owner/name`, or a network address. Local paths
/// and `file://` would reach other services' checkouts on this server.
pub fn valid_repo(repo: &str, provider: bool) -> bool {
    let r = repo.trim();
    if r.is_empty() || r.starts_with('-') || r.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if provider {
        return !r.contains("..") && !r.contains(['?', '#', ':', '\\']);
    }
    ["https://", "http://", "ssh://", "git://"].iter().any(|p| r.starts_with(p))
        || r.split_once(':').is_some_and(|(user_host, path)| user_host.contains('@') && !user_host.contains('/') && !path.is_empty())
}

/// Whether an http(s) repository address carries credentials (`https://user:token@host/...`),
/// which everyone who can see the service would read; providers and deploy keys keep them sealed.
pub fn url_has_credentials(repo: &str) -> bool {
    let Some(rest) = repo.strip_prefix("https://").or_else(|| repo.strip_prefix("http://")) else { return false };
    rest.split(['/', '?', '#']).next().is_some_and(|authority| authority.contains('@'))
}

/// The address as it may appear in logs: without credentials.
fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) if url_has_credentials(url) => {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!("{scheme}://{host}/{path}")
        }
        _ => url.to_string(),
    }
}

/// Builds and Compose read a checkout as root, so it must not hold symlinks that lead out of it
/// (to /etc/tugport, say, or another service's files).
fn check_links(root: &Path) -> Result<()> {
    use std::path::Component;
    fn walk(root: &Path, dir: &Path) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                let target = std::fs::read_link(&path)?;
                let mut at = dir.to_path_buf();
                let mut inside = !target.is_absolute();
                for c in target.components() {
                    match c {
                        Component::Normal(n) => at.push(n),
                        Component::CurDir => {}
                        Component::ParentDir => inside &= at.pop() && at.starts_with(root),
                        _ => inside = false,
                    }
                }
                if !inside || !at.starts_with(root) {
                    bail!("the repository has a symlink that points outside it: {} -> {}", path.strip_prefix(root).unwrap_or(&path).display(), target.display());
                }
            } else if kind.is_dir() && entry.file_name() != ".git" {
                walk(root, &path)?;
            }
        }
        Ok(())
    }
    walk(root, root)
}

/// Check out a service's Git source for a deploy (its commit, else its ref, else the branch) and
/// record the commit on the deploy. Returns the folder at the source's path and the commit.
pub async fn checkout(app: &App, s: &Service, d: &Deploy, log: &(dyn Fn(&str) + Sync)) -> Result<(PathBuf, String)> {
    let Source::Git { provider, repo, branch, path, submodules, .. } = &s.spec.source else { bail!("this service does not deploy from Git") };
    let git_ref = d.commit_sha.as_deref().or(d.git_ref.as_deref()).unwrap_or(branch);
    let dest = app.paths.sources().join(&s.id).join("repo");
    let (sha, msg) = fetch(app, s, provider.as_deref(), repo, git_ref, *submodules, &dest, log).await?;
    app.db.exec("UPDATE deploys SET commit_sha = ?1, commit_message = ?2 WHERE id = ?3", params![sha, msg, d.id])?;
    let dir = util::inside(&dest, path).filter(|d| d.is_dir()).ok_or_else(|| anyhow!("the path {path} does not exist in the repository"))?;
    Ok((dir, sha))
}

/// Fetch `git_ref` (branch, tag or commit) into `dest` and check it out. Returns (sha, message).
/// `dest` persists between deploys: the first fetch is shallow, later ones bring only new commits,
/// and a commit that is already there is not fetched at all.
#[allow(clippy::too_many_arguments)]
async fn fetch(app: &App, s: &Service, provider_id: Option<&str>, repo: &str, git_ref: &str, submodules: bool, dest: &Path, log: &(dyn Fn(&str) + Sync)) -> Result<(String, String)> {
    // Every source of a ref (the spec, webhooks, the API, migration) passes through here.
    if !valid_ref(git_ref) {
        bail!("{git_ref:?} is not a branch, tag or commit name");
    }
    if !valid_repo(repo, provider_id.is_some()) {
        bail!("{repo:?} is not a repository Tugport can fetch: use an https://, ssh:// or git@host: address");
    }
    let provider = provider_id.map(|id| provider(app, id)?.ok_or_else(|| anyhow!("the Git provider of this service was removed"))).transpose()?;
    let url = provider.as_ref().map_or_else(|| repo.to_string(), |p| p.clone_url(repo));
    // Scoped to the provider's address, so submodules hosted elsewhere never receive the token,
    // and passed in the environment, which other users cannot read (unlike argv).
    let header = match &provider {
        Some(p) => {
            let entry = format!("http.{}/.extraheader={}", p.base_url.trim_end_matches('/'), clone_header(p, repo).await?);
            Some(format!("'{}'", entry.replace('\'', "'\\''")))
        }
        None => None,
    };
    let key_path = app.paths.run.join("keys").join(format!("{}.key", s.id));
    let ssh_cmd = deploy_key_ssh(app, s, &key_path)?;
    let git = |args: &[&str]| {
        let mut c = Command::new("git");
        if let Some(h) = &header {
            c.env("GIT_CONFIG_PARAMETERS", h);
        }
        // Network transports only, for submodules too: no local paths, file:// or ext::.
        c.arg("-C").arg(dest).env("GIT_TERMINAL_PROMPT", "0").env("GIT_ALLOW_PROTOCOL", "https:http:ssh:git");
        // A detached auto-gc rewrites .git/shallow under the next command ("shallow file has changed").
        c.args(["-c", "gc.autoDetach=false", "-c", "maintenance.autoDetach=false"]).args(args);
        if let Some(ssh) = &ssh_cmd {
            c.env("GIT_SSH_COMMAND", ssh);
        }
        c
    };
    let run = |args: &[&str]| crate::proc::output(git(args));
    let fetched: Result<(String, String)> = async {
        let fresh = !dest.join(".git").exists();
        if fresh {
            std::fs::create_dir_all(dest)?;
            run(&["init", "-q"]).await?;
            run(&["remote", "add", "origin", &url]).await?;
        } else {
            run(&["remote", "set-url", "origin", &url]).await?;
        }
        let commit = format!("{git_ref}^{{commit}}");
        if is_sha(git_ref) && !fresh && run(&["cat-file", "-e", &commit]).await.is_ok() {
            log(&format!("{} is already here", &git_ref[..7]));
            run(&["checkout", "-q", "--force", git_ref]).await?;
        } else {
            log(&format!("Fetching {git_ref} from {}", redact(&url)));
            // --depth on an existing shallow clone downloads the whole tree again, so only the first fetch has it.
            let args: &[&str] = if fresh { &["fetch", "-q", "--depth", "1", "--no-tags", "origin", git_ref] } else { &["fetch", "-q", "--no-tags", "origin", git_ref] };
            if let Err(e) = crate::proc::run_streamed(git(args), log).await {
                // Full SHAs not at a branch tip need an unshallow fetch on some hosts.
                log(&format!("Fetch failed ({e}); fetching all branches"));
                crate::proc::run_streamed(git(&["fetch", "-q", "--no-tags", "origin", "+refs/heads/*:refs/remotes/origin/*"]), log).await?;
                run(&["checkout", "-q", "--force", git_ref]).await?;
            } else {
                run(&["checkout", "-q", "--force", "FETCH_HEAD"]).await?;
            }
        }
        run(&["clean", "-ffdxq"]).await?;
        if submodules && dest.join(".gitmodules").exists() {
            crate::proc::run_streamed(git(&["submodule", "update", "--init", "--recursive", "--depth", "1", "--jobs", "8"]), log).await?;
        }
        check_links(dest)?;
        Ok((run(&["rev-parse", "HEAD"]).await?.trim().to_string(), run(&["log", "-1", "--format=%B"]).await?.trim().to_string()))
    }
    .await;
    // The key stays on disk only while git runs, whether or not the fetch worked.
    if ssh_cmd.is_some() {
        let _ = std::fs::remove_file(&key_path);
    }
    let (sha, msg) = fetched?;
    log(&format!("Checked out {} {}", sha.get(..7).unwrap_or(&sha), msg.lines().next().unwrap_or("")));
    Ok((sha, msg))
}

/// A full commit id: one that may already be in the clone.
fn is_sha(r: &str) -> bool {
    r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Post the deploy state on the commit (shows up as a check next to the commit).
pub async fn report(app: &App, s: &Service, d: &Deploy, state: &str, description: &str) {
    let Source::Git { provider: Some(pid), repo, .. } = &s.spec.source else { return };
    let Some(sha) = d.commit_sha.as_ref().or(d.git_ref.as_ref()).filter(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())) else { return };
    let Ok(Some(p)) = provider(app, pid) else { return };
    let target = app.settings().panel_domain.map(|h| format!("https://{h}/services/{}/deploys/{}", s.id, d.id));
    let res: Result<()> = async {
        let token = repo_token(&p, repo).await?;
        let req = match p.kind {
            Kind::Gitlab => {
                let gl = match state {
                    "success" => "success",
                    "failure" => "failed",
                    _ => "running",
                };
                api(&p, Method::POST, &format!("/projects/{}/statuses/{sha}", util::urlencode(repo)), &token)?
                    .json(&json!({ "state": gl, "name": "tugport", "description": description, "target_url": target }))
            }
            _ => api(&p, Method::POST, &format!("/repos/{repo}/statuses/{sha}"), &token)?
                .json(&json!({ "state": state, "description": description, "context": "tugport", "target_url": target })),
        };
        send(req).await?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::debug!("could not report deploy status to {}: {e:#}", p.name);
    }
}

fn hmac_hex(secret: &str, body: &[u8]) -> String {
    hex::encode(util::hmac_sha256(secret, body))
}

pub fn verify_webhook(p: &Provider, headers: &axum::http::HeaderMap, body: &[u8]) -> bool {
    let Some(secret) = p.secret() else { return false };
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let sig = match p.kind {
        Kind::GithubApp | Kind::GithubToken => h("x-hub-signature-256").and_then(|s| s.strip_prefix("sha256=")),
        Kind::Forgejo => h("x-forgejo-signature").or(h("x-gitea-signature")),
        Kind::Gitlab => return h("x-gitlab-token").is_some_and(|t| util::ct_eq(t, secret)),
    };
    sig.is_some_and(|sig| util::ct_eq(sig, hmac_hex(secret, body)))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Push {
    pub repo: String,
    pub branch: String,
    pub sha: String,
    pub message: String,
    pub pusher: String,
    /// Files touched by the pushed commits; None when the host did not list them.
    pub changed: Option<Vec<String>>,
}

pub fn parse_push(headers: &axum::http::HeaderMap, body: &[u8]) -> Option<Push> {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    let event = h("x-github-event").or(h("x-forgejo-event")).or(h("x-gitea-event")).or(h("x-gitlab-event"))?;
    if event != "push" && event != "Push Hook" {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    let branch = v["ref"].as_str()?.strip_prefix("refs/heads/")?.to_string();
    let sha = v["checkout_sha"].as_str().or(v["after"].as_str())?.to_string();
    if sha.chars().all(|c| c == '0') || v["deleted"].as_bool() == Some(true) {
        return None;
    }
    let repo = v["repository"]["full_name"].as_str().or(v["project"]["path_with_namespace"].as_str())?.to_string();
    let message = v["head_commit"]["message"]
        .as_str()
        .or_else(|| v["commits"].as_array()?.last()?["message"].as_str())
        .unwrap_or("")
        .to_string();
    let pusher = v["pusher"]["name"]
        .as_str()
        .or(v["pusher"]["login"].as_str())
        .or(v["pusher"]["username"].as_str())
        .or(v["user_username"].as_str())
        .unwrap_or("")
        .to_string();
    let changed = v["commits"].as_array().map(|commits| {
        commits
            .iter()
            .flat_map(|c| ["added", "modified", "removed"].iter().filter_map(move |k| c[*k].as_array()))
            .flatten()
            .filter_map(|f| f.as_str().map(String::from))
            .collect::<Vec<_>>()
    });
    // Hosts cap the commit list (GitHub sends at most 20); then we cannot know every file.
    let changed = changed.filter(|c| !c.is_empty() && v["commits"].as_array().is_some_and(|a| a.len() < 20));
    Some(Push { repo, branch, sha, message, pusher, changed })
}

/// Glob match: `*` within a path segment, `**` across segments; a plain path matches everything under it.
fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./").trim_start_matches('/');
    if !pattern.contains('*') {
        let p = pattern.trim_end_matches('/');
        return path == p || path.starts_with(&format!("{p}/"));
    }
    fn go(p: &[&str], s: &[&str]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&"**"), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(pp), Some(ss)) => seg(pp, ss) && go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    fn seg(p: &str, s: &str) -> bool {
        match p.split_once('*') {
            None => p == s,
            Some((pre, rest)) => {
                s.starts_with(pre) && (0..=s.len() - pre.len()).any(|i| s.is_char_boundary(pre.len() + i) && seg(rest, &s[pre.len() + i..]))
            }
        }
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    go(&p, &s)
}

/// Should this push redeploy a service with these watch paths?
pub fn push_touches(watch: &[String], changed: Option<&[String]>) -> bool {
    match changed {
        _ if watch.is_empty() => true,
        None => true,
        Some(files) => files.iter().any(|f| watch.iter().any(|w| path_matches(w, f))),
    }
}

/// Create the push webhook on a repository (providers without app-level webhooks).
pub async fn ensure_webhook(p: &Provider, repo: &str, hook_url: &str) -> Result<()> {
    let secret = p.secret().ok_or_else(|| anyhow!("provider has no webhook secret"))?;
    let config = json!({ "url": hook_url, "content_type": "json", "secret": secret });
    // (hooks path, where a hook keeps its address, the new hook)
    let (path, url_at, hook) = match p.kind {
        Kind::GithubApp => return Ok(()),
        Kind::GithubToken => (format!("/repos/{repo}/hooks"), "/config/url", json!({ "name": "web", "active": true, "events": ["push"], "config": config })),
        Kind::Forgejo => (format!("/repos/{repo}/hooks"), "/config/url", json!({ "type": "forgejo", "active": true, "events": ["push"], "config": config })),
        Kind::Gitlab => (format!("/projects/{}/hooks", util::urlencode(repo)), "/url", json!({ "url": hook_url, "push_events": true, "token": secret })),
    };
    let token = p.token().unwrap_or_default();
    let hooks: Vec<Value> = send(api(p, Method::GET, &path, token)?).await?.json().await?;
    if !hooks.iter().any(|h| h.pointer(url_at).and_then(Value::as_str) == Some(hook_url)) {
        send(api(p, Method::POST, &path, token)?.json(&hook)).await?;
    }
    Ok(())
}

/// The `key` field of every item of a JSON list.
fn names(list: &Value, key: &str) -> Vec<String> {
    list.as_array().into_iter().flatten().filter_map(|r| r[key].as_str().map(String::from)).collect()
}

/// Repositories the provider can see, for the repository picker.
pub async fn repos(p: &Provider) -> Result<Vec<String>> {
    let (path, key) = match p.kind {
        Kind::GithubApp => {
            let jwt = app_jwt(p)?;
            let installs: Value = send(api(p, Method::GET, "/app/installations", &jwt)?).await?.json().await?;
            let mut out = vec![];
            for i in installs.as_array().into_iter().flatten() {
                let Some(inst_id) = i["id"].as_i64() else { continue };
                let Ok((token, _)) = installation_access(p, &jwt, inst_id).await else { continue };
                let v: Value = send(api(p, Method::GET, "/installation/repositories?per_page=100", &token)?).await?.json().await?;
                out.extend(names(&v["repositories"], "full_name"));
            }
            return Ok(out);
        }
        Kind::GithubToken => ("/user/repos?per_page=100&sort=pushed", "full_name"),
        Kind::Forgejo => ("/user/repos?limit=50", "full_name"),
        Kind::Gitlab => ("/projects?membership=true&per_page=100&order_by=last_activity_at", "path_with_namespace"),
    };
    let list: Value = send(api(p, Method::GET, path, p.token().unwrap_or_default())?).await?.json().await?;
    Ok(names(&list, key))
}

/// Turn a GitHub App manifest code into a stored provider.
pub async fn finish_github_manifest(app: &App, provider_id: &str, code: &str) -> Result<(String, String)> {
    let v: Value = send(http()?.post(format!("https://api.github.com/app-manifests/{code}/conversions")).header("Accept", "application/vnd.github+json"))
        .await
        .context("GitHub did not accept the app manifest code")?
        .json()
        .await?;
    let slug = v["slug"].as_str().ok_or_else(|| anyhow!("no app slug"))?.to_string();
    let config = json!({
        "app_id": v["id"], "slug": slug, "pem": v["pem"], "webhook_secret": v["webhook_secret"],
        "client_id": v["client_id"], "client_secret": v["client_secret"],
    });
    let name = format!("GitHub ({slug})");
    save_provider(app, Some(provider_id), Kind::GithubApp, &name, "https://github.com", &config)?;
    if v["webhook_secret"].is_null() {
        bail!("GitHub created the app without a webhook secret");
    }
    Ok((slug.clone(), format!("https://github.com/apps/{slug}/installations/new")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_full_commit_ids_are_looked_up_locally() {
        assert!(is_sha("fba652009ad96134cb51580f5c8d1daa185d05d5"));
        assert!(!is_sha("fba6520"));
        assert!(!is_sha("main"));
        assert!(!is_sha("zba652009ad96134cb51580f5c8d1daa185d05d5"));
    }

    #[test]
    fn signs_github_app_jwt() {
        // Throwaway key generated for this test only. jsonwebtoken 10 panics at signing time
        // when no crypto backend feature is enabled, so this guards the Cargo.toml features.
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEogIBAAKCAQEA1ATxd6ef6QobMzro4IvKTcy0my+5lSuKfS6vWhSXESnFdKMZ\ndUebG8dyCaVX6ybyDCNBiogjQDp6W11Ap8TQKnrtcmGp4n+hsybmsIpxOlWD1zrB\nNbact1oLhwFaM6M7fNTaQtOh1OlFrur39SAX1yF23Isp0RSXrKZn37j7e6SS81bv\nh2XUp54AL50D78/VRyssylU3JGZxtCSgi4K9owNVNx+rLew0WbDXSMtjn3ZIqeSs\nVCPhq2qPe9k/EWK5zmA+ptw7fIBfNCuDzMVsGpO4JCtwS6V6K8UWw0tBVNg/kXkD\nWwMY9eNsQO9RhR+Rd/BKi2rLTyceaTKg9K4nYQIDAQABAoH/Hm+XOlYV3p5UyUXr\nqFBo+kIoqx5uxbs6Zj4NbiEGjrAwaE4BmZRt787xz/CsblVo6dUl0303kkDSz8rT\nPUJDn9WFwKXEfX9U9yLeevlkx7r+8ia9FbjhRnDmtrq6+C62m3Ty4CUBVk8mcbXJ\nYqb4xSlIpuBTYHBUVbSNtNAZSeZTPcwR3LyxEE5AtISpqw42ouBznVuqW/14nIFw\n1hNZE9AEJfs01PkvyvLzcNidJZnGPgOspscXItCEiPLVhZ6vEJ6bF6O07LYOvkNN\nD76LkGwqqZ7xqoRkjbLPPtN0sorEdZdc0+aAH1PsnvN1ainEHsZGpb30gIBw/+7T\nUYABAoGBAPN/N/G2IuhebOdEjianmg3VM3n8sIyqNWCa0Uom01LuC3d2jJ8dP9ky\nEtihE3CLTarxrmw0TKXIxNV1fWeBlR239AVnVhN+esxK61KkumtowExG1Gg1VEUi\n8ggSdwDK4P0mpQNVqUH75kOnex47oqTD+YJDjjHwatq0UXAyo9dhAoGBAN7n8wcn\n8LJ2fUKjgrknV7E2IOHiIBoo/76Kr0YUdmfmWnrCVV15pwB8AU3KOHCD3Inqcgq5\nbQ81KQJ5iEq0sg29PAOG9UUmpepNxDzUIqP872tFkX/+NO7h4DOnLWhuHdE103+2\n5J1ZOcksm7NaDfNvw0K6rN1UlliYJlOUPFABAoGAONptb4x9XhAeEWmaX0Am4i/z\nkvoDJvTe/a1oTI4ALY7TMpOFhelIbSgZ6mbBqTd9bPp9JVqYwuS7wLNsEmzZKlU3\nLsPP/0E63tRnWaaIuGd+7tBi6+n3wSTFqkjVbGPyYGlTghHQMU3ll8pVHtwBv8La\nUSYA33wpFuOfWDkatEECgYEAo0d4p/dDwxY0bwnjCcmd3LtvnsbLCR6bJ3HjLgxz\nJ2x/Poj3iv8/q4UqV1cXvKN6erP3UY++KXOhVKS9hyuRearyOQs0wrCqvFS2uHAT\n59excPyGtiD5mTpvxqZhBqwSv1sUT+siz3e6cnhwnRIMQhPODiU71uzNLJxXBsYA\n4AECgYEAm2c/OVQzzlkZiKKos/C6IQi4qM+YZrdhE0PgWt0C58xFjAXOwDn4up8Y\nyr4TgXilwIdVBc4jlJggIUuu5oA0DcePumEJ9BftHsT5dWzkbBCrxHx0mY/IPjBB\n7pctdHyUsyUlw5rmrsjneELy4REEntBkmDz4gP+WO7np4S9kE7M=\n-----END RSA PRIVATE KEY-----";
        let p = Provider { id: "p".into(), kind: Kind::GithubApp, name: "gh".into(), base_url: "https://github.com".into(), config: json!({ "app_id": 1, "pem": pem }) };
        assert_eq!(app_jwt(&p).unwrap().split('.').count(), 3);
    }

    #[test]
    fn globs() {
        assert!(path_matches("frontend", "frontend/src/app.ts"));
        assert!(path_matches("frontend/", "frontend/x"));
        assert!(!path_matches("frontend", "frontend-old/x"));
        assert!(path_matches("apps/*/src/**", "apps/api/src/a/b.rs"));
        assert!(path_matches("**/*.md", "docs/a/readme.md"));
        assert!(path_matches("*.toml", "Cargo.toml"));
        assert!(!path_matches("*.toml", "crates/Cargo.toml"));
        assert!(push_touches(&[], Some(&["x".into()])));
        assert!(push_touches(&["api".into()], None));
        assert!(!push_touches(&["api".into()], Some(&["web/index.html".into()])));
    }

    #[test]
    fn parses_github_and_forgejo_pushes() {
        let body = br#"{"ref":"refs/heads/main","after":"0123456789abcdef0123456789abcdef01234567","repository":{"full_name":"o/r"},
            "head_commit":{"message":"fix it"},"pusher":{"name":"nikita"},"commits":[{"added":["a.rs"],"modified":["api/b.rs"],"removed":[]}]}"#;
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-github-event", "push".parse().unwrap());
        let p = parse_push(&h, body).unwrap();
        assert_eq!(p.repo, "o/r");
        assert_eq!(p.branch, "main");
        assert_eq!(p.changed.unwrap(), vec!["a.rs", "api/b.rs"]);
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forgejo-event", "push".parse().unwrap());
        assert_eq!(parse_push(&h, body).unwrap().pusher, "nikita");
        let tag = br#"{"ref":"refs/tags/v1","after":"01","repository":{"full_name":"o/r"}}"#;
        assert!(parse_push(&h, tag).is_none());
    }

    #[test]
    fn refuses_option_refs_and_local_repos() {
        for ok in ["main", "feature/x-1", "v1.2.3", "refs/tags/v1", "0123456789abcdef0123456789abcdef01234567", "release@2"] {
            assert!(valid_ref(ok), "{ok}");
        }
        for bad in ["", "-x", "--upload-pack=x", "--pathspec-from-file=/etc/passwd", "+main", "a..b", "a b", "x:y", "/abs", "a\nb"] {
            assert!(!valid_ref(bad), "{bad}");
        }
        for ok in ["https://github.com/o/r.git", "git@github.com:o/r.git", "ssh://git@host:2222/o/r"] {
            assert!(valid_repo(ok, false), "{ok}");
        }
        for bad in ["/var/lib/tugport/src/x/repo", "file:///etc", "ext::sh -c x", "-u", "../x", "C:/x"] {
            assert!(!valid_repo(bad, false), "{bad}");
        }
        assert!(valid_repo("group/sub/name", true));
        assert!(!valid_repo("o/../../x", true));
        assert!(url_has_credentials("https://user:ghp_secret@github.com/o/r.git"));
        assert!(url_has_credentials("https://token@git.example.com/o/r"));
        assert!(!url_has_credentials("https://github.com/o/r.git"));
        assert!(!url_has_credentials("https://github.com/o/r@v1"));
        assert!(!url_has_credentials("git@github.com:o/r.git"));
        assert_eq!(redact("https://user:ghp_secret@github.com/o/r.git"), "https://github.com/o/r.git");
    }

    #[test]
    fn finds_symlinks_that_leave_the_checkout() {
        let root = std::env::temp_dir().join(format!("tp-links-{}", util::id()));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::os::unix::fs::symlink("../b", root.join("a/up")).unwrap();
        std::os::unix::fs::symlink("b/missing", root.join("a/dangling")).unwrap();
        assert!(check_links(&root).is_ok());
        std::os::unix::fs::symlink("../../..", root.join("a/b/out")).unwrap();
        assert!(check_links(&root).is_err());
        std::fs::remove_file(root.join("a/b/out")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("abs")).unwrap();
        assert!(check_links(&root).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn verifies_signatures() {
        let p = Provider { id: "x".into(), kind: Kind::Forgejo, name: "f".into(), base_url: "https://git.example.com".into(), config: json!({ "webhook_secret": "s3cret" }) };
        let body = b"{}";
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-forgejo-signature", hmac_hex("s3cret", body).parse().unwrap());
        assert!(verify_webhook(&p, &h, body));
        h.insert("x-forgejo-signature", hmac_hex("wrong", body).parse().unwrap());
        assert!(!verify_webhook(&p, &h, body));
    }
}
