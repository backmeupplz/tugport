//! Git hosting: providers (GitHub App, GitHub token, Forgejo/Gitea, GitLab), clones,
//! commit statuses, and push webhooks.

use crate::app::App;
use crate::deploy::Deploy;
use crate::model::Source;
use crate::store::Service;
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
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
    pub fn api(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        match self.kind {
            Kind::GithubApp | Kind::GithubToken if base == "https://github.com" => "https://api.github.com".into(),
            Kind::GithubApp | Kind::GithubToken => format!("{base}/api/v3"),
            Kind::Forgejo => format!("{base}/api/v1"),
            Kind::Gitlab => format!("{base}/api/v4"),
        }
    }
    pub fn clone_url(&self, repo: &str) -> String {
        format!("{}/{}.git", self.base_url.trim_end_matches('/'), repo.trim_end_matches(".git"))
    }
}

pub fn provider(app: &App, id: &str) -> Result<Option<Provider>> {
    let row: Option<(String, String, String, String, Vec<u8>)> = app
        .db
        .conn()
        .query_row("SELECT id, kind, name, base_url, config FROM git_providers WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    let Some((id, kind, name, base_url, config)) = row else { return Ok(None) };
    let config = serde_json::from_str(&app.keys.open_str(&format!("git:{id}"), &config)?)?;
    Ok(Some(Provider { id, kind: Kind::parse(&kind).ok_or_else(|| anyhow!("unknown provider kind {kind}"))?, name, base_url, config }))
}

pub fn providers(app: &App) -> Result<Vec<Value>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, kind, name, base_url, created_at FROM git_providers ORDER BY created_at")?;
    Ok(stmt
        .query_map([], |r| {
            Ok(json!({ "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "name": r.get::<_, String>(2)?,
                       "base_url": r.get::<_, String>(3)?, "created_at": r.get::<_, i64>(4)? }))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn save_provider(app: &App, id: Option<&str>, kind: Kind, name: &str, base_url: &str, config: &Value) -> Result<String> {
    let id = id.map(String::from).unwrap_or_else(util::id);
    let sealed = app.keys.seal_str(&format!("git:{id}"), &config.to_string())?;
    app.db.conn().execute(
        "INSERT INTO git_providers (id, kind, name, base_url, config, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, base_url = excluded.base_url, config = excluded.config",
        params![id, kind.as_str(), name, base_url.trim_end_matches('/'), sealed, util::now()],
    )?;
    Ok(id)
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("tugport/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

static TOKENS: Mutex<Option<HashMap<String, (String, i64)>>> = Mutex::new(None);

fn app_jwt(p: &Provider) -> Result<String> {
    let app_id = p.config.get("app_id").and_then(|v| v.as_i64()).ok_or_else(|| anyhow!("GitHub App id missing"))?;
    let pem = p.config.get("pem").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("GitHub App key missing"))?;
    let now = util::now();
    let claims = json!({ "iat": now - 60, "exp": now + 540, "iss": app_id.to_string() });
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())?;
    Ok(jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &claims, &key)?)
}

/// Short-lived installation token for a repository the GitHub App can see.
pub async fn installation_token(p: &Provider, repo: &str) -> Result<String> {
    let owner = repo.split('/').next().unwrap_or(repo).to_lowercase();
    let cache_key = format!("{}:{owner}", p.id);
    if let Some((t, exp)) = crate::util::lock(&TOKENS).get_or_insert_with(HashMap::new).get(&cache_key)
        && *exp > util::now() + 300 {
            return Ok(t.clone());
        }
    let jwt = app_jwt(p)?;
    let api = p.api();
    let inst: Value = http()?
        .get(format!("{api}/repos/{repo}/installation"))
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("the GitHub App is not installed on {repo}"))?
        .json()
        .await?;
    let inst_id = inst["id"].as_i64().ok_or_else(|| anyhow!("no installation id"))?;
    let (token, exp) = installation_access(p, &jwt, inst_id).await?;
    crate::util::lock(&TOKENS).get_or_insert_with(HashMap::new).insert(cache_key, (token.clone(), exp));
    Ok(token)
}

/// A token for one installation of the GitHub App, and when it expires.
async fn installation_access(p: &Provider, jwt: &str, inst_id: i64) -> Result<(String, i64)> {
    let tok: Value = http()?
        .post(format!("{}/app/installations/{inst_id}/access_tokens", p.api()))
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = tok["token"].as_str().ok_or_else(|| anyhow!("no installation token"))?.to_string();
    let exp = chrono::DateTime::parse_from_rfc3339(tok["expires_at"].as_str().unwrap_or("")).map(|d| d.timestamp()).unwrap_or(util::now() + 3000);
    Ok((token, exp))
}

/// HTTP Basic credentials for cloning, passed per command so they never land in .git/config.
async fn clone_header(p: &Provider, repo: &str) -> Result<String> {
    let (user, pass) = match p.kind {
        Kind::GithubApp => ("x-access-token".to_string(), installation_token(p, repo).await?),
        Kind::GithubToken => ("x-access-token".to_string(), p.token().unwrap_or_default().to_string()),
        Kind::Forgejo => ("tugport".to_string(), p.token().unwrap_or_default().to_string()),
        Kind::Gitlab => ("oauth2".to_string(), p.token().unwrap_or_default().to_string()),
    };
    Ok(format!("Authorization: Basic {}", crate::crypto::b64(format!("{user}:{pass}").as_bytes())))
}

pub fn deploy_key_path(app: &App, service_id: &str) -> std::path::PathBuf {
    app.paths.run.join("keys").join(format!("{service_id}.key"))
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

/// Builds and Compose read a checkout as root, so it must not hold symlinks that lead out of it
/// (to /etc/tugport, say, or another service's files).
pub fn check_links(root: &Path) -> Result<()> {
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

/// Fetch `git_ref` (branch, tag or commit) into `dest` and check it out. Returns (sha, message).
pub async fn checkout(app: &App, s: &Service, provider_id: Option<&str>, repo: &str, git_ref: &str, dest: &Path, log: &(dyn Fn(&str) + Sync)) -> Result<(String, String)> {
    // Every source of a ref (the spec, webhooks, the API, migration) passes through here.
    if !valid_ref(git_ref) {
        bail!("{git_ref:?} is not a branch, tag or commit name");
    }
    if !valid_repo(repo, provider_id.is_some()) {
        bail!("{repo:?} is not a repository Tugport can fetch: use an https://, ssh:// or git@host: address");
    }
    let provider = match provider_id {
        Some(id) => Some(provider(app, id)?.ok_or_else(|| anyhow!("the Git provider of this service was removed"))?),
        None => None,
    };
    let url = provider.as_ref().map(|p| p.clone_url(repo)).unwrap_or_else(|| repo.to_string());
    // Scoped to the provider's address, so submodules hosted elsewhere never receive the token,
    // and passed in the environment, which other users cannot read (unlike argv).
    let header = match &provider {
        Some(p) => {
            let entry = format!("http.{}/.extraheader={}", p.base_url.trim_end_matches('/'), clone_header(p, repo).await?);
            Some(format!("'{}'", entry.replace('\'', "'\\''")))
        }
        None => None,
    };
    let ssh_cmd = match app.db.get_state(&format!("deploykey:{}", s.id))? {
        Some(sealed) => {
            let key = app.keys.open_str(&format!("deploykey:{}", s.id), &crate::crypto::unb64(&sealed)?)?;
            let path = deploy_key_path(app, &s.id);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
                f.write_all(key.as_bytes())?;
                if !key.ends_with('\n') {
                    f.write_all(b"\n")?;
                }
            }
            let known = app.paths.data.join("known_hosts");
            Some(format!("ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}", path.display(), known.display()))
        }
        None => None,
    };
    let git = |args: &[&str]| {
        let mut c = Command::new("git");
        if let Some(h) = &header {
            c.env("GIT_CONFIG_PARAMETERS", h);
        }
        // Network transports only, for submodules too: no local paths, file:// or ext::.
        c.arg("-C").arg(dest).args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_ALLOW_PROTOCOL", "https:http:ssh:git");
        if let Some(ssh) = &ssh_cmd {
            c.env("GIT_SSH_COMMAND", ssh);
        }
        c
    };
    if !dest.join(".git").exists() {
        std::fs::create_dir_all(dest)?;
        crate::proc::output(git(&["init", "-q"])).await?;
        crate::proc::output(git(&["remote", "add", "origin", &url])).await?;
    } else {
        crate::proc::output(git(&["remote", "set-url", "origin", &url])).await?;
    }
    log(&format!("Fetching {git_ref} from {}", url));
    let fetched = crate::proc::run_streamed(git(&["fetch", "-q", "--depth", "50", "--no-tags", "origin", git_ref]), log).await;
    if let Err(e) = fetched {
        // Full SHAs not at a branch tip need an unshallow fetch on some hosts.
        log(&format!("Shallow fetch failed ({e}); fetching all branches"));
        crate::proc::run_streamed(git(&["fetch", "-q", "--no-tags", "origin", "+refs/heads/*:refs/remotes/origin/*"]), log).await?;
        crate::proc::output(git(&["checkout", "-q", "--force", git_ref])).await?;
    } else {
        crate::proc::output(git(&["checkout", "-q", "--force", "FETCH_HEAD"])).await?;
    }
    crate::proc::output(git(&["clean", "-ffdxq"])).await?;
    if dest.join(".gitmodules").exists() {
        crate::proc::run_streamed(git(&["submodule", "update", "--init", "--recursive", "--depth", "1"]), log).await?;
    }
    check_links(dest)?;
    let sha = crate::proc::output(git(&["rev-parse", "HEAD"])).await?.trim().to_string();
    let msg = crate::proc::output(git(&["log", "-1", "--format=%B"])).await?.trim().to_string();
    if let Some(ssh) = &ssh_cmd {
        let _ = ssh;
        let _ = std::fs::remove_file(deploy_key_path(app, &s.id));
    }
    log(&format!("Checked out {} {}", &sha[..sha.len().min(7)], msg.lines().next().unwrap_or("")));
    Ok((sha, msg))
}

/// Post the deploy state on the commit (shows up as a check next to the commit).
pub async fn report(app: &App, s: &Service, d: &Deploy, state: &str, description: &str) {
    let Source::Git { provider: Some(pid), repo, .. } = &s.spec.source else { return };
    let Some(sha) = d.commit_sha.as_ref().or(d.git_ref.as_ref()).filter(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit())) else { return };
    let Ok(Some(p)) = provider(app, pid) else { return };
    let target = app.settings().panel_domain.map(|h| format!("https://{h}/services/{}/deploys/{}", s.id, d.id));
    let res: Result<()> = async {
        let client = http()?;
        let req = match p.kind {
            Kind::GithubApp | Kind::GithubToken => {
                let token = match p.kind {
                    Kind::GithubApp => installation_token(&p, repo).await?,
                    _ => p.token().unwrap_or_default().to_string(),
                };
                client
                    .post(format!("{}/repos/{repo}/statuses/{sha}", p.api()))
                    .bearer_auth(token)
                    .json(&json!({ "state": state, "description": description, "context": "tugport", "target_url": target }))
            }
            Kind::Forgejo => client
                .post(format!("{}/repos/{repo}/statuses/{sha}", p.api()))
                .header("Authorization", format!("token {}", p.token().unwrap_or_default()))
                .json(&json!({ "state": state, "description": description, "context": "tugport", "target_url": target })),
            Kind::Gitlab => {
                let gl = match state {
                    "success" => "success",
                    "failure" => "failed",
                    _ => "running",
                };
                client
                    .post(format!("{}/projects/{}/statuses/{sha}", p.api(), urlencode(repo)))
                    .header("PRIVATE-TOKEN", p.token().unwrap_or_default())
                    .json(&json!({ "state": gl, "name": "tugport", "description": description, "target_url": target }))
            }
        };
        req.send().await?.error_for_status()?;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::debug!("could not report deploy status to {}: {e:#}", p.name);
    }
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn hmac_hex(secret: &str, body: &[u8]) -> Option<String> {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    Some(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_webhook(p: &Provider, headers: &axum::http::HeaderMap, body: &[u8]) -> bool {
    let Some(secret) = p.secret() else { return false };
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok());
    match p.kind {
        Kind::GithubApp | Kind::GithubToken => {
            let expected = hmac_hex(secret, body);
            h("x-hub-signature-256").and_then(|s| s.strip_prefix("sha256=")).is_some_and(|sig| expected.is_some_and(|e| util::ct_eq(sig, &e)))
        }
        Kind::Forgejo => {
            let expected = hmac_hex(secret, body);
            h("x-forgejo-signature").or(h("x-gitea-signature")).is_some_and(|sig| expected.is_some_and(|e| util::ct_eq(sig, &e)))
        }
        Kind::Gitlab => h("x-gitlab-token").is_some_and(|t| util::ct_eq(t, secret)),
    }
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
pub fn path_matches(pattern: &str, path: &str) -> bool {
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
    let client = http()?;
    match p.kind {
        Kind::GithubApp => Ok(()),
        Kind::GithubToken => {
            let hooks: Vec<Value> =
                client.get(format!("{}/repos/{repo}/hooks", p.api())).bearer_auth(p.token().unwrap_or_default()).send().await?.error_for_status()?.json().await?;
            if hooks.iter().any(|h| h["config"]["url"].as_str() == Some(hook_url)) {
                return Ok(());
            }
            client
                .post(format!("{}/repos/{repo}/hooks", p.api()))
                .bearer_auth(p.token().unwrap_or_default())
                .json(&json!({ "name": "web", "active": true, "events": ["push"], "config": { "url": hook_url, "content_type": "json", "secret": secret } }))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
        Kind::Forgejo => {
            let auth = format!("token {}", p.token().unwrap_or_default());
            let hooks: Vec<Value> =
                client.get(format!("{}/repos/{repo}/hooks", p.api())).header("Authorization", &auth).send().await?.error_for_status()?.json().await?;
            if hooks.iter().any(|h| h["config"]["url"].as_str() == Some(hook_url)) {
                return Ok(());
            }
            client
                .post(format!("{}/repos/{repo}/hooks", p.api()))
                .header("Authorization", &auth)
                .json(&json!({ "type": "forgejo", "active": true, "events": ["push"], "config": { "url": hook_url, "content_type": "json", "secret": secret } }))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
        Kind::Gitlab => {
            let pid = urlencode(repo);
            let hooks: Vec<Value> = client
                .get(format!("{}/projects/{pid}/hooks", p.api()))
                .header("PRIVATE-TOKEN", p.token().unwrap_or_default())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if hooks.iter().any(|h| h["url"].as_str() == Some(hook_url)) {
                return Ok(());
            }
            client
                .post(format!("{}/projects/{pid}/hooks", p.api()))
                .header("PRIVATE-TOKEN", p.token().unwrap_or_default())
                .json(&json!({ "url": hook_url, "push_events": true, "token": secret }))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
    }
}

/// Repositories the provider can see, for the repository picker.
pub async fn repos(p: &Provider) -> Result<Vec<String>> {
    let client = http()?;
    let names = |v: Vec<Value>, key: &str| -> Vec<String> { v.into_iter().filter_map(|r| r[key].as_str().map(String::from)).collect() };
    Ok(match p.kind {
        Kind::GithubApp => {
            let jwt = app_jwt(p)?;
            let installs: Vec<Value> = client.get(format!("{}/app/installations", p.api())).bearer_auth(&jwt).send().await?.error_for_status()?.json().await?;
            let mut out = vec![];
            for i in installs {
                let Some(inst_id) = i["id"].as_i64() else { continue };
                let Ok((token, _)) = installation_access(p, &jwt, inst_id).await else { continue };
                let v: Value = client
                    .get(format!("{}/installation/repositories?per_page=100", p.api()))
                    .bearer_auth(token)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                out.extend(names(v["repositories"].as_array().cloned().unwrap_or_default(), "full_name"));
            }
            out
        }
        Kind::GithubToken => names(
            client.get(format!("{}/user/repos?per_page=100&sort=pushed", p.api())).bearer_auth(p.token().unwrap_or_default()).send().await?.error_for_status()?.json().await?,
            "full_name",
        ),
        Kind::Forgejo => names(
            client
                .get(format!("{}/user/repos?limit=50", p.api()))
                .header("Authorization", format!("token {}", p.token().unwrap_or_default()))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
            "full_name",
        ),
        Kind::Gitlab => names(
            client
                .get(format!("{}/projects?membership=true&per_page=100&order_by=last_activity_at", p.api()))
                .header("PRIVATE-TOKEN", p.token().unwrap_or_default())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?,
            "path_with_namespace",
        ),
    })
}

/// Turn a GitHub App manifest code into a stored provider.
pub async fn finish_github_manifest(app: &App, provider_id: &str, code: &str) -> Result<(String, String)> {
    let v: Value = http()?
        .post(format!("https://api.github.com/app-manifests/{code}/conversions"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
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
        h.insert("x-forgejo-signature", hmac_hex("s3cret", body).unwrap().parse().unwrap());
        assert!(verify_webhook(&p, &h, body));
        h.insert("x-forgejo-signature", hmac_hex("wrong", body).unwrap().parse().unwrap());
        assert!(!verify_webhook(&p, &h, body));
    }
}
