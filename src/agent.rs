//! `tugport node`: the agent on every server that joined a panel. It brings up the mesh,
//! lets the panel drive its Docker over the mesh, and runs a local proxy and metrics
//! sampler. Only the panel's mesh address, with the shared key, can talk to it.

use crate::app::{App, AppState};
use crate::mesh::{self, Peer};
use crate::nodes::{AGENT_KEY, AGENT_PORT, DOCKER_PORT};
use crate::{logs, paths, proxy, secrets, util};
use anyhow::{Context, Result, bail};
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize, Clone)]
pub struct AgentConfig {
    pub node_id: String,
    pub panel_url: String,
    pub mesh_ip: String,
    pub secret: String,
    pub peers: Vec<Peer>,
}

fn config_path(p: &paths::Paths) -> std::path::PathBuf {
    p.etc.join("agent.json")
}

fn save_config(p: &paths::Paths, c: &AgentConfig) -> Result<()> {
    util::write_private(&config_path(p), serde_json::to_string_pretty(c)?.as_bytes())
}

struct Ctx {
    app: AppState,
    config: tokio::sync::Mutex<AgentConfig>,
    secret: String,
    started: std::time::Instant,
}

type Shared = Arc<Ctx>;
type Failure = (StatusCode, Json<Value>);

/// `tugport mesh-up` on a server that joined a panel (false when it has not joined).
pub fn mesh_up(paths: &paths::Paths) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(config_path(paths)) else {
        return Ok(false);
    };
    let config: AgentConfig = serde_json::from_str(&text)?;
    mesh::boot(&paths.etc.join("wg.key"), &config.mesh_ip, &config.peers)?;
    Ok(true)
}

pub async fn run() -> Result<()> {
    use tracing_subscriber::prelude::*;
    let paths = paths::Paths::from_env();
    paths.ensure()?;
    let selflog = logs::SelfLog::new(paths.logs().join("_tugport"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "tugport=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::fmt::layer().json().with_ansi(false).with_writer(logs::SelfLogHandle(selflog.clone())))
        .init();
    let config: AgentConfig = serde_json::from_str(&std::fs::read_to_string(config_path(&paths)).context("this server has not joined a panel")?)?;
    if let Err(e) = mesh::install_boot_hook() {
        tracing::warn!("making Docker wait for the mesh at boot: {e:#}");
    }
    mesh::guard()?;
    mesh::up(&paths.etc.join("wg.key"), &config.mesh_ip)?;
    mesh::set_peers(&config.peers)?;

    // A small local App so the proxy and the metrics sampler work as they do on the panel.
    // It has no secrets of its own: the key below only seals what never leaves this process.
    let mut key = Zeroizing::new([0u8; 32]);
    rand::RngCore::fill_bytes(&mut rand::rng(), key.as_mut());
    let db = crate::db::Db::open(&paths.data.join("agent.db"))?;
    let app = App::new(paths, db, crate::crypto::Keyring::new(&key), bollard::Docker::connect_with_local_defaults()?, selflog, 1)?;
    *util::write(&app.proxy.me) = config.node_id.clone();
    crate::install_init_binary(&app)?;
    if let Some(table) = std::fs::read_to_string(app.paths.data.join("agent-routes.json")).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        app.proxy.set_table(table);
    }
    let (mesh_ip, secret) = (config.mesh_ip.clone(), config.secret.clone());
    let ctx: Shared = Arc::new(Ctx { app: app.clone(), config: tokio::sync::Mutex::new(config), secret: secret.clone(), started: std::time::Instant::now() });

    tokio::spawn(crate::metrics::sample_loop(app.clone(), false));
    // Docker restarts and firewall reloads drop iptables rules; keep containers off the mesh,
    // and log a warning when swarm mode breaks, even while the panel is away.
    let docker = app.docker.clone();
    tokio::spawn(util::every(60, "guarding the mesh", move || {
        let docker = docker.clone();
        async move {
            crate::nodes::swarm_problem(crate::nodes::guard_mesh(&docker).await.as_ref());
            Ok(())
        }
    }));
    tokio::spawn(docker_proxy(mesh_ip.clone(), secret));
    {
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = proxy::serve(app, 80, 443).await {
                tracing::error!("proxy stopped: {e:#}");
            }
        });
    }
    let router = Router::new()
        .route("/agent/health", get(health))
        .route("/agent/peers", put(peers))
        .route("/agent/routes", put(routes))
        .route("/agent/certs", put(certs))
        .route("/agent/secrets/{container}", put(put_secrets).delete(delete_secrets))
        .route("/agent/check", post(check))
        .route("/agent/metrics", get(metrics))
        .route("/agent/leave", post(leave))
        .layer(axum::middleware::from_fn_with_state(ctx.clone(), only_panel))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind((mesh_ip.as_str(), AGENT_PORT)).await.with_context(|| format!("listening on {mesh_ip}:{AGENT_PORT}"))?;
    tracing::info!("tugport node {} on {mesh_ip}", crate::VERSION);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

fn from_panel(peer: &SocketAddr) -> bool {
    peer.ip().to_string() == mesh::PANEL_IP
}

async fn only_panel(State(ctx): State<Shared>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let key = req.headers().get(AGENT_KEY).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !from_panel(&peer) || !util::ct_eq(key, &ctx.secret) {
        return (StatusCode::FORBIDDEN, Json(json!({ "error": "only the panel may call this agent" }))).into_response();
    }
    next.run(req).await
}

/// The panel drives this server's Docker over the mesh: a connection is spliced to the
/// Docker socket once its first request carries the agent key.
async fn docker_proxy(mesh_ip: String, secret: String) {
    let Ok(listener) = tokio::net::TcpListener::bind((mesh_ip.as_str(), DOCKER_PORT)).await else {
        tracing::error!("cannot listen on {mesh_ip}:{DOCKER_PORT}");
        return;
    };
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                // Out of descriptors, say: wait rather than spin.
                tracing::debug!("accepting a docker connection: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !from_panel(&peer) {
            continue;
        }
        let secret = secret.clone();
        tokio::spawn(async move {
            if let Err(e) = docker_conn(stream, &secret).await {
                tracing::debug!("docker connection from the panel: {e:#}");
            }
        });
    }
}

async fn docker_conn(mut stream: tokio::net::TcpStream, secret: &str) -> Result<()> {
    let mut head = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let end = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("closed");
            }
            head.extend_from_slice(&chunk[..n]);
            if let Some(i) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(i);
            }
            if head.len() > 64 * 1024 {
                bail!("request head too large");
            }
        }
    })
    .await??;
    if !request_has_key(&head[..end], secret) {
        stream.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let mut docker = tokio::net::UnixStream::connect("/var/run/docker.sock").await?;
    docker.write_all(&head).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut docker).await?;
    Ok(())
}

fn request_has_key(head: &[u8], secret: &str) -> bool {
    String::from_utf8_lossy(head).lines().skip(1).any(|l| {
        l.split_once(':').is_some_and(|(k, v)| k.trim().eq_ignore_ascii_case(AGENT_KEY) && util::ct_eq(v.trim(), secret))
    })
}

fn fail(e: impl Into<anyhow::Error>) -> Failure {
    let e = e.into();
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") })))
}

async fn health(State(ctx): State<Shared>) -> Json<Value> {
    let mut info = crate::nodes::host_info(&ctx.app).await;
    info["uptime_s"] = json!(ctx.started.elapsed().as_secs());
    info["traffic"] = json!(*util::lock(&ctx.app.proxy.traffic));
    Json(info)
}

async fn peers(State(ctx): State<Shared>, Json(peers): Json<Vec<Peer>>) -> Result<Json<Value>, Failure> {
    mesh::set_peers(&peers).map_err(fail)?;
    let mut c = ctx.config.lock().await;
    c.peers = peers;
    save_config(&ctx.app.paths, &c).map_err(fail)?;
    Ok(Json(json!({ "ok": true })))
}

async fn routes(State(ctx): State<Shared>, Json(table): Json<proxy::Table>) -> Result<Json<Value>, Failure> {
    let text = serde_json::to_string(&table).map_err(fail)?;
    std::fs::write(ctx.app.paths.data.join("agent-routes.json"), text).map_err(fail)?;
    ctx.app.proxy.set_table(table);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct CertIn {
    host: String,
    cert: String,
    key: String,
}

/// Certificates stay in memory only; the panel sends them again after a restart.
async fn certs(State(ctx): State<Shared>, Json(certs): Json<Vec<CertIn>>) -> Json<Value> {
    let mut loaded = 0;
    for c in certs {
        if let Ok(ck) = proxy::certified_key(&c.cert, &c.key) {
            ctx.app.proxy.set_cert(&c.host, ck);
            loaded += 1;
        }
    }
    Json(json!({ "loaded": loaded }))
}

#[derive(Deserialize)]
struct SecretsIn {
    env: BTreeMap<String, String>,
    #[serde(default)]
    files: Vec<(String, String)>,
}

/// A Docker container name, safe as a file name under /run.
fn container_name(c: &str) -> bool {
    c.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && !c.starts_with('.')
}

/// Secrets land in this server's memory-only /run, exactly as on the panel's server.
async fn put_secrets(State(ctx): State<Shared>, Path(container): Path<String>, Json(b): Json<SecretsIn>) -> Result<Json<Value>, Failure> {
    if !container_name(&container) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "bad container name" }))));
    }
    let env: BTreeMap<String, Zeroizing<String>> = b.env.into_iter().map(|(k, v)| (k, Zeroizing::new(v))).collect();
    let path = secrets::materialize(&ctx.app, &container, &env).map_err(fail)?;
    let files: Vec<(String, Zeroizing<String>)> = b.files.into_iter().map(|(t, c)| (t, Zeroizing::new(c))).collect();
    let mounted = secrets::materialize_files(&ctx.app, &container, &files).map_err(fail)?;
    Ok(Json(json!({ "env": path, "files": mounted })))
}

async fn delete_secrets(State(ctx): State<Shared>, Path(container): Path<String>) -> Json<Value> {
    if container_name(&container) {
        secrets::forget(&ctx.app, &container);
    }
    Json(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct CheckIn {
    ip: String,
    port: u16,
    /// HTTP path; none means a TCP connect.
    path: Option<String>,
    #[serde(default)]
    strict: bool,
}

/// Health checks for replicas here, which the panel cannot reach directly.
async fn check(Json(q): Json<CheckIn>) -> Json<Value> {
    let ok = match &q.path {
        Some(path) => crate::runtime::http_ok(&q.ip, q.port, path, q.strict).await,
        None => tokio::net::TcpStream::connect((q.ip.as_str(), q.port)).await.is_ok(),
    };
    Json(json!({ "ok": ok }))
}

async fn metrics(State(ctx): State<Shared>) -> Json<Value> {
    Json(serde_json::to_value(&*util::lock(&ctx.app.metrics.latest)).unwrap_or_default())
}

/// Leave the panel: drop out of the swarm and the mesh, then stop for good.
async fn leave(State(ctx): State<Shared>) -> Json<Value> {
    let _ = std::process::Command::new("docker").args(["swarm", "leave", "--force"]).output();
    let _ = std::fs::remove_file(config_path(&ctx.app.paths));
    let _ = std::fs::remove_file(ctx.app.paths.etc.join("wg.key"));
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = std::process::Command::new("ip").args(["link", "del", mesh::IFACE]).output();
        let _ = std::process::Command::new("systemctl").args(["disable", "--now", "tugport-node"]).output();
        std::process::exit(0);
    });
    Json(json!({ "ok": true }))
}

/// `tugport install --join <panel> --token <t>`: register with the panel and set up the mesh.
pub async fn join(panel_url: &str, token: &str) -> Result<AgentConfig> {
    let paths = paths::Paths::from_env();
    paths.ensure()?;
    let (private, public) = mesh::keypair()?;
    util::write_private(&paths.etc.join("wg.key"), private.as_bytes())?;
    let endpoint = crate::public_ip().map(|ip| format!("{ip}:{}", mesh::PORT));
    let hostname = std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).unwrap_or_default();
    let client = util::http(60).build()?;
    // The token never leaves this server; see nodes::join_secret.
    let req = crate::nodes::JoinRequest {
        token_id: hex::encode(crate::nodes::join_secret(token, "id")),
        proof: hex::encode(crate::nodes::join_secret(token, &format!("proof:{public}"))),
        name: hostname,
        wg_public_key: public,
        endpoint,
        info: json!({ "arch": std::env::consts::ARCH }),
    };
    let resp = client
        .post(format!("{}/api/nodes/join", panel_url.trim_end_matches('/')))
        .json(&req)
        .send()
        .await
        .with_context(|| format!("reaching the panel at {panel_url}"))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("the panel refused to add this server: {}", v["error"].as_str().unwrap_or(status.as_str()));
    }
    let envelope: crate::nodes::JoinEnvelope = serde_json::from_value(v)?;
    let opened = crate::crypto::Keyring::new(&crate::nodes::join_secret(token, "response"))
        .open_str("join", &crate::crypto::unb64(&envelope.sealed)?)
        .context("the panel's answer did not open with this token")?;
    let r: crate::nodes::JoinResponse = serde_json::from_str(&opened)?;
    mesh::guard()?;
    mesh::up(&paths.etc.join("wg.key"), &r.mesh_ip)?;
    mesh::set_peers(&r.peers)?;
    if !crate::nodes::swarm_active(&bollard::Docker::connect_with_local_defaults()?).await? {
        let panel = format!("{}:2377", mesh::PANEL_IP);
        // The first packets may wait for the WireGuard handshake.
        for attempt in 1.. {
            let mut c = tokio::process::Command::new("docker");
            c.args(["swarm", "join", "--token", &r.swarm_token, "--advertise-addr", &r.mesh_ip, "--data-path-addr", &r.mesh_ip, &panel]);
            match crate::proc::output(c).await {
                Ok(_) => break,
                Err(e) if attempt == 10 => return Err(e.context("joining the overlay network failed")),
                Err(_) => tokio::time::sleep(Duration::from_secs(3)).await,
            }
        }
    }
    let config = AgentConfig { node_id: r.node_id, panel_url: panel_url.to_string(), mesh_ip: r.mesh_ip, secret: r.secret, peers: r.peers };
    save_config(&paths, &config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::request_has_key;

    #[test]
    fn secrets_only_for_plain_container_names() {
        use super::container_name;
        assert!(container_name("web-1.abc_x"));
        assert!(!container_name(".hidden") && !container_name("../etc") && !container_name("a/b"));
    }

    #[test]
    fn docker_requests_need_the_key() {
        let ok = b"GET /_ping HTTP/1.1\r\nHost: x\r\nX-Tugport-Agent-Key: s3cret\r\n";
        assert!(request_has_key(ok, "s3cret"));
        assert!(!request_has_key(ok, "other"));
        assert!(!request_has_key(b"GET /_ping HTTP/1.1\r\nHost: x\r\n", "s3cret"));
        // The request line itself never counts.
        assert!(!request_has_key(b"GET /x-tugport-agent-key: s3cret HTTP/1.1\r\n", "s3cret"));
    }
}
