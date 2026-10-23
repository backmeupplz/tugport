//! More servers: the panel's view of every node, joining, placement, and talking to node
//! agents over the WireGuard mesh. The panel's own server is the node "local".

use crate::app::{App, AppState};
use crate::mesh::{self, Peer};
use crate::model::{Mount, Source};
use crate::store::{self, Service};
use crate::{runtime, util};
use anyhow::{Context, Result, anyhow, bail};
use bollard::query_parameters::ListContainersOptions;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const AGENT_PORT: u16 = 7948;
pub const DOCKER_PORT: u16 = 7947;
pub const REGISTRY_PORT: u16 = 5000;
/// A server unreachable this long has its movable services started elsewhere.
const RESCHEDULE_AFTER_S: i64 = 120;

pub struct NodeHandle {
    pub id: String,
    pub name: String,
    pub mesh_ip: Option<String>,
    pub local: bool,
    pub docker: bollard::Docker,
    secret: Option<String>,
    pub region: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Default)]
pub struct Nodes(RwLock<HashMap<String, Arc<NodeHandle>>>);

impl App {
    pub fn node(&self, id: &str) -> Option<Arc<NodeHandle>> {
        util::read(&self.nodes.0).get(id).cloned()
    }

    pub fn all_nodes(&self) -> Vec<Arc<NodeHandle>> {
        let mut v: Vec<_> = util::read(&self.nodes.0).values().cloned().collect();
        v.sort_by(|a, b| b.local.cmp(&a.local).then(a.name.cmp(&b.name)));
        v
    }

    /// Docker on the server a container runs on.
    pub fn docker_on(&self, node: &str) -> Result<bollard::Docker> {
        if node == "local" {
            return Ok(self.docker.clone());
        }
        self.node(node).map(|n| n.docker.clone()).ok_or_else(|| anyhow!("server {node} is not part of this panel"))
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct MeshState {
    private_key: String,
    public_key: String,
    endpoint: String,
    /// Overlay networks need IPsec when an existing swarm advertises a public address.
    encrypt_overlay: bool,
}

fn mesh_state(app: &App) -> Result<Option<MeshState>> {
    match app.db.get_state("mesh")? {
        Some(sealed) => Ok(Some(serde_json::from_str(&app.keys.open_str("mesh", &crate::crypto::unb64(&sealed)?)?)?)),
        None => Ok(None),
    }
}

/// More than one server: services use overlay networks and may run anywhere.
pub fn mesh_enabled(app: &App) -> bool {
    app.db.get_state("mesh").ok().flatten().is_some()
}

pub fn encrypt_overlay(app: &App) -> bool {
    mesh_state(app).ok().flatten().is_some_and(|m| m.encrypt_overlay)
}

/// Docker on another server through its agent; every connection proves itself with the node key.
fn remote_docker(ip: &str, secret: &str) -> Result<bollard::Docker> {
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    // An unreachable server must not stall everything that lists containers.
    connector.set_connect_timeout(Some(Duration::from_secs(3)));
    let client = Arc::new(hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector));
    let key = hyper::header::HeaderValue::from_str(secret)?;
    Ok(bollard::Docker::connect_with_custom_transport(
        move |mut req: bollard::BollardRequest| {
            req.headers_mut().insert("x-tugport-agent-key", key.clone());
            let client = client.clone();
            async move { client.request(req).await.map_err(bollard::errors::Error::from) }
        },
        Some(format!("http://{ip}:{DOCKER_PORT}")),
        300,
        bollard::API_DEFAULT_VERSION,
    )?)
}

fn node_secret(app: &App, id: &str) -> Result<String> {
    let key = format!("nodekey:{id}");
    let sealed = app.db.get_state(&key)?.ok_or_else(|| anyhow!("no agent key stored"))?;
    Ok(app.keys.open_str(&key, &crate::crypto::unb64(&sealed)?)?.to_string())
}

/// Load nodes from the database (the local node always exists).
#[allow(clippy::type_complexity)]
pub fn load(app: &App) -> Result<()> {
    app.db.conn().execute(
        "INSERT OR IGNORE INTO nodes (id, name, is_local, status, created_at) VALUES ('local', ?1, 1, 'online', ?2)",
        params![hostname(), util::now()],
    )?;
    let rows: Vec<(String, String, bool, Option<String>, Option<String>, String)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT id, name, is_local, mesh_ip, region, labels FROM nodes")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let mut map = HashMap::new();
    for (id, name, local, mesh_ip, region, labels) in rows {
        let (docker, secret) = if local {
            (app.docker.clone(), None)
        } else {
            let connected = node_secret(app, &id).and_then(|secret| {
                let ip = mesh_ip.as_deref().ok_or_else(|| anyhow!("no mesh address"))?;
                Ok((remote_docker(ip, &secret)?, Some(secret)))
            });
            match connected {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("server {name}: {e:#}");
                    continue;
                }
            }
        };
        let labels = serde_json::from_str(&labels).unwrap_or_default();
        map.insert(id.clone(), Arc::new(NodeHandle { id, name, mesh_ip, local, docker, secret, region, labels }));
    }
    *util::write(&app.nodes.0) = map;
    Ok(())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_string()).ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "main".into())
}

async fn sh(cmd: &str) -> Result<String> {
    let mut c = tokio::process::Command::new("sh");
    c.args(["-c", cmd]);
    crate::proc::output(c).await
}

/// Turn on the mesh on the panel's server: WireGuard, swarm over the mesh, image registry.
pub async fn ensure_mesh(app: &App) -> Result<()> {
    if !mesh::available() {
        bail!("WireGuard tools are missing; install wireguard-tools and try again");
    }
    let state = match mesh_state(app)? {
        Some(s) => s,
        None => {
            let (private_key, public_key) = mesh::keypair()?;
            let ip = tokio::task::spawn_blocking(crate::public_address).await?.ok_or_else(|| anyhow!("could not find this server's address"))?;
            let swarm = sh("docker info --format '{{.Swarm.LocalNodeState}} {{.Swarm.NodeAddr}}'").await.unwrap_or_default();
            let encrypt_overlay = swarm.starts_with("active") && !swarm.contains(mesh::PANEL_IP);
            MeshState { private_key, public_key, endpoint: format!("{ip}:{}", mesh::PORT), encrypt_overlay }
        }
    };
    let key_file = app.paths.etc.join("wg.key");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&key_file)?;
        f.write_all(state.private_key.as_bytes())?;
    }
    mesh::guard()?;
    mesh::up(&key_file, mesh::PANEL_IP)?;
    let swarm = sh("docker info --format '{{.Swarm.LocalNodeState}}'").await?;
    if swarm.trim() != "active" {
        sh(&format!("docker swarm init --advertise-addr {ip} --data-path-addr {ip} --listen-addr {ip}:2377", ip = mesh::PANEL_IP))
            .await
            .context("starting swarm mode for overlay networks")?;
    }
    ensure_registry().await?;
    let first = !mesh_enabled(app);
    let sealed = app.keys.seal_str("mesh", &serde_json::to_string(&state)?)?;
    app.db.set_state("mesh", &crate::crypto::b64(&sealed))?;
    app.db.conn().execute("UPDATE nodes SET mesh_ip = ?1 WHERE id = 'local'", [mesh::PANEL_IP])?;
    set_peers(app)?;
    load(app)?;
    if first {
        // Running containers join their project's new overlay network, so services on other
        // servers reach them by name without a redeploy.
        runtime::move_to_overlays(app).await?;
    }
    Ok(())
}

/// After a reboot: the mesh interface and its peers come back before anything uses them,
/// then the registry, which Docker may have restarted without its ports.
pub async fn restore_mesh(app: &App) -> Result<()> {
    if !mesh_enabled(app) {
        return Ok(());
    }
    mesh::guard()?;
    mesh::up(&app.paths.etc.join("wg.key"), mesh::PANEL_IP)?;
    set_peers(app)?;
    ensure_registry().await
}

/// The registry itself answers on localhost (not just anything that holds the port).
async fn registry_answers() -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(1)).no_proxy().build() else {
        return false;
    };
    client.get(format!("http://127.0.0.1:{REGISTRY_PORT}/v2/")).send().await.is_ok_and(|r| r.headers().contains_key("docker-distribution-api-version"))
}

/// A private registry on the mesh so servers pull only the image layers they lack.
async fn ensure_registry() -> Result<()> {
    // A running container is not enough: after a Docker restart it can run with no published
    // ports (both are published together, so localhost tells). One that just started gets a
    // few seconds to answer before it is replaced.
    if sh("docker inspect -f '{{.State.Running}}' tugport-registry").await.map(|s| s.trim() == "true").unwrap_or(false) {
        for _ in 0..5 {
            if registry_answers().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        tracing::warn!("the image registry runs but does not answer on port {REGISTRY_PORT}; starting it again");
    }
    let _ = sh("docker rm -f tugport-registry").await;
    sh(&format!(
        "docker run -d --name tugport-registry --restart unless-stopped --label tugport.system=registry \
         -p {ip}:{port}:5000 -p 127.0.0.1:{port}:5000 -v tugport-registry:/var/lib/registry registry:2",
        ip = mesh::PANEL_IP,
        port = REGISTRY_PORT
    ))
    .await
    .context("starting the image registry")?;
    Ok(())
}

/// The registry only carries images between servers; start it over empty now and then.
// ponytail: wipes the whole transfer cache; delete unused tags instead if re-sending layers gets slow.
pub async fn reset_registry(app: &App) {
    if !mesh_enabled(app) || !app.deployer.idle() {
        return;
    }
    let _ = sh("docker rm -f tugport-registry && docker volume rm tugport-registry").await;
    if let Err(e) = ensure_registry().await {
        tracing::warn!("restarting the image registry: {e:#}");
    }
}

/// `tugport mesh-up` on the panel's server (false when the mesh is off). Docker runs it before
/// Tugport starts, so it only reads the database and leaves migrations to the panel.
pub fn mesh_up(paths: &crate::paths::Paths) -> Result<bool> {
    let key_file = paths.etc.join("wg.key");
    if !paths.db().exists() || !key_file.exists() {
        return Ok(false);
    }
    let conn = rusqlite::Connection::open_with_flags(paths.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    if conn.query_row("SELECT 1 FROM state WHERE key = 'mesh'", [], |_| Ok(())).optional()?.is_none() {
        return Ok(false);
    }
    mesh::boot(&key_file, mesh::PANEL_IP, &peers_for(&conn, None)?)?;
    Ok(true)
}

fn peers_for(conn: &rusqlite::Connection, exclude: Option<&str>) -> Result<Vec<Peer>> {
    let mut stmt = conn.prepare("SELECT id, wg_public_key, address, mesh_ip FROM nodes WHERE is_local = 0 AND wg_public_key IS NOT NULL")?;
    let rows: Vec<(String, String, Option<String>, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter(|(id, ..)| Some(id.as_str()) != exclude)
        .map(|(_, public_key, endpoint, mesh_ip)| Peer { public_key, endpoint, mesh_ip })
        .collect())
}

fn set_peers(app: &App) -> Result<()> {
    let peers = peers_for(&app.db.conn(), None)?;
    mesh::set_peers(&peers)
}

/// The peer list each agent should have: the panel plus every other server.
fn peers_of_agent(app: &App, node_id: &str) -> Result<Vec<Peer>> {
    let st = mesh_state(app)?.ok_or_else(|| anyhow!("the mesh is not set up"))?;
    // The panel address this server joined through works for it; ours may be private (EC2).
    let endpoint = app.db.get_state(&format!("panelep:{node_id}"))?.unwrap_or(st.endpoint);
    let mut peers = vec![Peer { public_key: st.public_key, endpoint: Some(endpoint), mesh_ip: mesh::PANEL_IP.into() }];
    peers.extend(peers_for(&app.db.conn(), Some(node_id))?);
    Ok(peers)
}

#[derive(Serialize)]
pub struct JoinToken {
    pub token: String,
    pub command: String,
    pub expires_at: i64,
}

pub async fn join_token(app: &App, panel_url: &str) -> Result<JoinToken> {
    join_token_for(app, panel_url, None).await
}

/// Keys a join derives from its one-time token. The token itself never crosses the
/// network: a new server sends `id`, proves it holds the token with `proof` (bound to its
/// WireGuard key), and the panel's answer is sealed with `response`. Plain HTTP is safe.
pub fn join_secret(token: &str, label: &str) -> Result<[u8; 32]> {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(token.trim().as_bytes()).map_err(|e| anyhow!("{e}"))?;
    mac.update(label.as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

/// A one-hour join command; `cloud` ties it to a machine Tugport is creating.
pub async fn join_token_for(app: &App, panel_url: &str, cloud: Option<&str>) -> Result<JoinToken> {
    ensure_mesh(app).await?;
    let token = util::token(40);
    let expires_at = util::now() + 3600;
    let key = format!("jointoken:{}", hex::encode(join_secret(&token, "id")?));
    let sealed = crate::crypto::b64(&app.keys.seal_str(&key, &token)?);
    app.db.set_state(&key, &json!({ "expires_at": expires_at, "cloud": cloud, "token": sealed }).to_string())?;
    Ok(JoinToken {
        command: format!("curl -fsSL https://tugport.com/install | sh -s -- --join {panel_url} --token {token}"),
        token,
        expires_at,
    })
}

#[derive(Serialize, Deserialize)]
pub struct JoinRequest {
    /// `join_secret(token, "id")`, hex: finds the token without revealing it.
    pub token_id: String,
    /// `join_secret(token, "proof:<wg_public_key>")`, hex.
    pub proof: String,
    pub name: String,
    pub wg_public_key: String,
    /// `ip:port` other servers can reach this one on (None behind NAT).
    pub endpoint: Option<String>,
    #[serde(default)]
    pub info: Value,
}

#[derive(Serialize, Deserialize)]
pub struct JoinResponse {
    pub node_id: String,
    pub mesh_ip: String,
    pub secret: String,
    pub peers: Vec<Peer>,
    pub swarm_token: String,
    pub registry: String,
}

/// Addresses only reachable inside one network (private, shared, link-local, loopback).
fn local_only(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || (v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Where other servers reach a new one: what it says, unless that is a private address
/// and we saw it come from a public one (a cloud machine behind 1:1 NAT, like EC2).
fn reachable_endpoint(claimed: Option<&str>, seen: Option<std::net::IpAddr>) -> Option<String> {
    let claimed_ip = claimed.and_then(|c| c.parse::<SocketAddr>().ok()).map(|a| a.ip());
    match (claimed_ip, seen.map(|s| s.to_canonical())) {
        (Some(c), Some(s)) if local_only(c) && !local_only(s) => Some(SocketAddr::new(s, mesh::PORT).to_string()),
        (None, Some(s)) if !local_only(s) => Some(SocketAddr::new(s, mesh::PORT).to_string()),
        _ => claimed.map(String::from),
    }
}

pub async fn join(app: &AppState, req: JoinRequest, panel_ip: Option<std::net::IpAddr>, seen: Option<std::net::IpAddr>) -> Result<JoinEnvelope> {
    let invalid = || anyhow!("this join command is not valid; make a new one in the panel");
    if req.token_id.len() != 64 || !req.token_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let key = format!("jointoken:{}", req.token_id);
    let stored: Value = app.db.get_state(&key)?.and_then(|v| serde_json::from_str(&v).ok()).ok_or_else(invalid)?;
    let token = app.keys.open_str(&key, &crate::crypto::unb64(stored["token"].as_str().unwrap_or_default())?).map_err(|_| invalid())?;
    let proof = hex::encode(join_secret(&token, &format!("proof:{}", req.wg_public_key))?);
    if !util::ct_eq(&proof, req.proof.trim()) {
        return Err(invalid());
    }
    app.db.conn().execute("DELETE FROM state WHERE key = ?1", [&key])?;
    if stored["expires_at"].as_i64().unwrap_or(0) < util::now() {
        bail!("this join command expired; make a new one in the panel");
    }
    let cloud = stored["cloud"].as_str().map(String::from);
    if crate::crypto::unb64(&req.wg_public_key).map(|k| k.len()).unwrap_or(0) != 32 {
        bail!("invalid WireGuard key");
    }
    let node_id = util::id();
    let secret = util::token(40);
    // One lock for picking the address and taking it, so simultaneous joins never share one.
    let (mesh_ip, name) = {
        let conn = app.db.conn();
        let used: Vec<String> = {
            let mut stmt = conn.prepare("SELECT mesh_ip FROM nodes WHERE mesh_ip IS NOT NULL")?;
            stmt.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect()
        };
        let index = (2..60000).find(|i| !used.contains(&mesh::mesh_ip(*i))).ok_or_else(|| anyhow!("the mesh is full"))?;
        let mesh_ip = mesh::mesh_ip(index);
        let mut name = server_name(&req.name).unwrap_or_else(|| format!("server-{index}"));
        let taken: bool = conn.query_row("SELECT COUNT(*) > 0 FROM nodes WHERE name = ?1", [&name], |r| r.get(0))?;
        if taken {
            name = format!("{name}-{index}");
        }
        conn.execute(
            "INSERT INTO nodes (id, name, is_local, address, mesh_ip, wg_public_key, status, info, created_at) VALUES (?1, ?2, 0, ?3, ?4, ?5, 'joining', ?6, ?7)",
            params![node_id, name, reachable_endpoint(req.endpoint.as_deref(), seen), mesh_ip, req.wg_public_key, req.info.to_string(), util::now()],
        )?;
        (mesh_ip, name)
    };
    let sealed = app.keys.seal_str(&format!("nodekey:{node_id}"), &secret)?;
    app.db.set_state(&format!("nodekey:{node_id}"), &crate::crypto::b64(&sealed))?;
    if let Some(ip) = panel_ip.filter(|ip| !ip.is_loopback()) {
        app.db.set_state(&format!("panelep:{node_id}"), &SocketAddr::new(ip, mesh::PORT).to_string())?;
    }
    if let Some(machine) = &cloud {
        // A machine Tugport created: it gets its region and its pool's labels.
        app.db.conn().execute(
            "UPDATE nodes SET region = (SELECT region FROM cloud_servers WHERE id = ?1),
               labels = COALESCE((SELECT p.labels FROM server_pools p JOIN cloud_servers c ON c.pool_id = p.id WHERE c.id = ?1), '{}')
             WHERE id = ?2",
            params![machine, node_id],
        )?;
        app.db.conn().execute("UPDATE cloud_servers SET node_id = ?1, status = 'joined' WHERE id = ?2", params![node_id, machine])?;
    }
    set_peers(app)?;
    load(app)?;
    let swarm_token = sh("docker swarm join-token -q worker").await?.trim().to_string();
    let peers = peers_of_agent(app, &node_id)?;
    // Everyone else learns about the new server.
    push_peers(app);
    app.audit(None, "node.join", None, None, Some(&name), Some(&mesh_ip));
    app.emit("nodes", None, None, json!({ "joined": name }));
    let response = JoinResponse { node_id, mesh_ip, secret, peers, swarm_token, registry: format!("{}:{REGISTRY_PORT}", mesh::PANEL_IP) };
    let sealed = crate::crypto::Keyring::new(&join_secret(&token, "response")?).seal_str("join", &serde_json::to_string(&response)?)?;
    Ok(JoinEnvelope { sealed: crate::crypto::b64(&sealed) })
}

/// The join answer, readable only by whoever holds the token.
#[derive(Serialize, Deserialize)]
pub struct JoinEnvelope {
    pub sealed: String,
}

/// A hostname as a server name (lowercase letters, digits and dashes, starting with a letter).
fn server_name(host: &str) -> Option<String> {
    let mut out = String::new();
    for c in host.trim().to_ascii_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && (out.is_empty() || out.ends_with('-'))) {
            out.push(c);
        }
    }
    let out = out.trim_end_matches('-').chars().take(30).collect::<String>().trim_end_matches('-').to_string();
    match out.chars().next() {
        Some(c) if c.is_ascii_lowercase() => Some(out),
        Some(_) => Some(format!("server-{out}")),
        None => None,
    }
}

/// Call a node agent over the mesh.
pub async fn agent(node: &NodeHandle, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
    let ip = node.mesh_ip.as_deref().ok_or_else(|| anyhow!("{} is not on the mesh", node.name))?;
    let secret = node.secret.as_deref().ok_or_else(|| anyhow!("{} has no agent key", node.name))?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    let mut req = client.request(method, format!("http://{ip}:{AGENT_PORT}{path}")).header("x-tugport-agent-key", secret);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.with_context(|| format!("reaching {}", node.name))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}: {}", node.name, v["error"].as_str().unwrap_or(status.as_str()));
    }
    Ok(v)
}

/// Where each node's proxy listens on the mesh (for forwarding between servers).
pub fn proxy_addresses(app: &App) -> HashMap<String, SocketAddr> {
    app.all_nodes()
        .iter()
        .filter_map(|n| Some((n.id.clone(), SocketAddr::new(n.mesh_ip.as_ref()?.parse().ok()?, 80))))
        .collect()
}

fn remote(app: &App) -> Vec<Arc<NodeHandle>> {
    app.all_nodes().into_iter().filter(|n| !n.local).collect()
}

/// Send the routing table to every server and wait until they have it.
pub async fn push_routes_now(app: &App) {
    let body = serde_json::to_value(&*app.proxy.table()).unwrap_or_default();
    let pushes = remote(app).into_iter().map(|n| {
        let body = body.clone();
        async move {
            if let Err(e) = agent(&n, reqwest::Method::PUT, "/agent/routes", Some(body)).await {
                tracing::debug!("pushing routes to {}: {e:#}", n.name);
            }
        }
    });
    futures_util::future::join_all(pushes).await;
}

pub fn push_routes(app: &App) {
    let table = app.proxy.table();
    for n in remote(app) {
        let body = serde_json::to_value(&*table).unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) = agent(&n, reqwest::Method::PUT, "/agent/routes", Some(body)).await {
                tracing::debug!("pushing routes to {}: {e:#}", n.name);
            }
        });
    }
}

pub fn push_peers(app: &AppState) {
    for n in remote(app) {
        let Ok(peers) = peers_of_agent(app, &n.id) else { continue };
        let body = serde_json::to_value(&peers).unwrap_or_default();
        tokio::spawn(async move {
            if let Err(e) = agent(&n, reqwest::Method::PUT, "/agent/peers", Some(body)).await {
                tracing::debug!("pushing peers to {}: {e:#}", n.name);
            }
        });
    }
}

/// Certificates go to every server so whichever one DNS points at can serve HTTPS.
pub async fn push_certs(app: &App) {
    let rows: Vec<(String, String, Vec<u8>)> = {
        let conn = app.db.conn();
        let Ok(mut stmt) = conn.prepare("SELECT host, cert_pem, key_pem FROM certificates WHERE cert_pem IS NOT NULL") else { return };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map(|m| m.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    };
    let certs: Vec<Value> = rows
        .into_iter()
        .filter_map(|(host, cert, key)| {
            let key = app.keys.open_str(&format!("cert:{host}"), &key).ok()?;
            Some(json!({ "host": host, "cert": cert, "key": key.as_str() }))
        })
        .collect();
    for n in remote(app) {
        if let Err(e) = agent(&n, reqwest::Method::PUT, "/agent/certs", Some(Value::Array(certs.clone()))).await {
            tracing::debug!("pushing certificates to {}: {e:#}", n.name);
        }
    }
}

/// Write a container's secrets on the server it runs on. Returns the env file and the
/// (host path, target) of each secret file there.
pub async fn put_secrets(app: &App, node: &NodeHandle, container: &str, s: &Service) -> Result<(String, Vec<(String, String)>)> {
    let mut env = store::resolved_env(app, s)?;
    runtime::add_port(app, s, &mut env)?;
    let files = store::files(app, &s.id)?;
    if node.local {
        let path = crate::secrets::materialize(app, container, &env)?;
        let mounted = crate::secrets::materialize_files(app, container, &files)?;
        return Ok((path.to_string_lossy().into(), mounted.into_iter().map(|(h, t)| (h.to_string_lossy().into(), t)).collect()));
    }
    let env: BTreeMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(t, c)| (t.as_str(), c.as_str())).collect();
    let v = agent(node, reqwest::Method::PUT, &format!("/agent/secrets/{container}"), Some(json!({ "env": env, "files": files }))).await?;
    let path = v["env"].as_str().ok_or_else(|| anyhow!("{} did not store the secrets", node.name))?.to_string();
    Ok((path, serde_json::from_value(v["files"].clone()).unwrap_or_default()))
}

pub async fn forget_secrets(app: &App, node: &str, container: &str) {
    match app.node(node) {
        Some(n) if !n.local => {
            let _ = agent(&n, reqwest::Method::DELETE, &format!("/agent/secrets/{container}"), None).await;
        }
        _ => crate::secrets::forget(app, container),
    }
}

/// The `docker` CLI aimed at a server, for the parts that shell out (backups). Its config
/// lives in memory-only /run and adds the agent key to every request.
pub fn docker_cli(app: &App, node: &str) -> Result<tokio::process::Command> {
    let mut c = tokio::process::Command::new("docker");
    let Some(n) = app.node(node).filter(|n| !n.local) else { return Ok(c) };
    let (Some(ip), Some(secret)) = (n.mesh_ip.as_deref(), n.secret.as_deref()) else { bail!("{} is not on the mesh", n.name) };
    let dir = app.paths.run.join("docker").join(&n.id);
    if !dir.join("config.json").exists() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(&dir)?;
        crate::paths::restrict(&dir, 0o700)?;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(dir.join("config.json"))?;
        f.write_all(json!({ "HttpHeaders": { "x-tugport-agent-key": secret } }).to_string().as_bytes())?;
    }
    c.env("DOCKER_HOST", format!("tcp://{ip}:{DOCKER_PORT}")).env("DOCKER_CONFIG", &dir);
    Ok(c)
}

/// The server a service keeps its data on (this one unless placed elsewhere).
pub fn home(app: &App, s: &Service) -> Result<Arc<NodeHandle>> {
    let id = app.db.get_state(&format!("pin:{}", s.id))?.unwrap_or_else(|| "local".into());
    app.node(&id).or_else(|| app.node("local")).ok_or_else(|| anyhow!("no server for {}", s.name))
}

/// Keep `task` running for every server: started for new servers, restarted when it ends.
pub async fn for_each_node<F, Fut>(app: AppState, task: F)
where
    F: Fn(AppState, Arc<NodeHandle>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let running: Arc<Mutex<HashSet<String>>> = Default::default();
    loop {
        for n in app.all_nodes() {
            if !util::lock(&running).insert(n.id.clone()) {
                continue;
            }
            let (fut, running) = (task(app.clone(), n.clone()), running.clone());
            tokio::spawn(async move {
                fut.await;
                util::lock(&running).remove(&n.id);
            });
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// What is wrong with this server's swarm mode, if anything. When Docker started before the
/// mesh it could not bind its mesh address: a manager stays in an error state and a worker
/// stays pending, deploys fail with "not a swarm manager", and only a Docker restart with the
/// mesh up recovers. A worker gets two minutes in pending, the time a normal join takes.
fn swarm_issue(state: Option<bollard::models::LocalNodeState>, error: Option<&str>, pending_for: Duration) -> Option<String> {
    use bollard::models::LocalNodeState;
    const FIX: &str = "If Docker started before the private network between servers (after a reboot), run `systemctl restart docker` on this server.";
    match state? {
        LocalNodeState::ERROR => Some(match error.map(str::trim).filter(|e| !e.is_empty()) {
            Some(e) => format!("Swarm mode failed on this server ({e}), so deploys that use it fail. {FIX}"),
            None => format!("Swarm mode failed on this server, so deploys that use it fail. {FIX}"),
        }),
        LocalNodeState::PENDING if pending_for >= Duration::from_secs(120) => Some(format!(
            "This server has been trying to rejoin the swarm for {} minutes, so services cannot run on it. Check that the panel's server is up and reachable over the private network. {FIX}",
            pending_for.as_secs() / 60
        )),
        _ => None,
    }
}

/// [`swarm_issue`] for Docker's current info, logged once when it starts and when it clears.
pub fn swarm_problem(info: Option<&bollard::models::SystemInfo>) -> Option<String> {
    use bollard::models::LocalNodeState;
    // (pending since, last problem)
    static SEEN: Mutex<(Option<std::time::Instant>, bool)> = Mutex::new((None, false));
    let swarm = info?.swarm.as_ref();
    let state = swarm.and_then(|s| s.local_node_state);
    let mut seen = util::lock(&SEEN);
    if state == Some(LocalNodeState::PENDING) {
        seen.0.get_or_insert_with(std::time::Instant::now);
    } else {
        seen.0 = None;
    }
    let problem = swarm_issue(state, swarm.and_then(|s| s.error.as_deref()), seen.0.map(|t| t.elapsed()).unwrap_or_default());
    match (&problem, seen.1) {
        (Some(p), false) => tracing::warn!("{p}"),
        (None, true) => tracing::info!("swarm mode works again"),
        _ => {}
    }
    seen.1 = problem.is_some();
    problem
}

/// What a server reports about itself (agents in their health, the panel for its own).
pub async fn host_info(app: &App) -> Value {
    let info = app.docker.info().await.ok();
    let snap = util::lock(&app.metrics.latest).clone();
    let swarm_problem = swarm_problem(info.as_ref());
    json!({
        "version": crate::VERSION,
        "docker": info.as_ref().and_then(|i| i.server_version.clone()),
        "boot_id": std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|b| b.trim().to_string()).unwrap_or_default(),
        "cpus": snap.cores.len(),
        "memory_total": snap.memory.total,
        "arch": std::env::consts::ARCH,
        "nvidia": info.as_ref().and_then(|i| i.runtimes.as_ref()).is_some_and(|r| r.contains_key("nvidia")),
        "dri": std::path::Path::new("/dev/dri").exists(),
        "swarm_node": info.and_then(|i| i.swarm).and_then(|s| s.node_id).filter(|id| !id.is_empty()),
        "swarm_problem": swarm_problem,
    })
}

/// The port the panel listens on. It is bound once (or inherited across a reload), so it can
/// differ from `panel_listen` until the next restart.
pub static PANEL_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// Keep containers on this server off the raw panel port, both the bound and the configured one.
pub async fn guard_panel(app: &App) -> Result<()> {
    let host = mesh::interfaces();
    let mut ports: Vec<u16> = PANEL_PORT.get().copied().into_iter().chain(app.settings().panel_port()).collect();
    ports.dedup();
    let bridges = runtime::bridge_interfaces(&app.docker).await.context("listing bridges to block containers from the panel");
    let guarded = mesh::guard_panel(&ports, &host, bridges.as_deref().ok());
    bridges.and(guarded)
}

/// Check every agent, collect its metrics, and keep routes, peers and certificates fresh.
pub async fn health_loop(app: AppState) {
    let mut ticks: u64 = 0;
    let mut panel_guard_error = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        ticks += 1;
        let own = host_info(&app).await;
        let _ = app.db.conn().execute("UPDATE nodes SET info = ?1, last_seen_at = ?2 WHERE id = 'local'", params![own.to_string(), util::now()]);
        // Docker restarts and firewall reloads drop iptables rules.
        if ticks.is_multiple_of(6) {
            // A server without a usable iptables would repeat the same error every minute.
            let error = guard_panel(&app).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            if !error.is_empty() && error != panel_guard_error {
                tracing::error!("{error}");
            }
            panel_guard_error = error;
            if mesh_enabled(&app)
                && let Err(e) = mesh::guard()
            {
                tracing::error!("{e:#}");
            }
        }
        for n in remote(&app) {
            let now = util::now();
            match agent(&n, reqwest::Method::GET, "/agent/health", None).await {
                Ok(h) => {
                    let (was, info): (String, String) =
                        app.db.conn().query_row("SELECT status, info FROM nodes WHERE id = ?1", [&n.id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or_default();
                    let old_boot = serde_json::from_str::<Value>(&info).ok().and_then(|i| i["boot_id"].as_str().map(String::from));
                    // A reboot empties the server's /run, and with it every container's secrets.
                    let rebooted = old_boot.is_some_and(|b| Some(b.as_str()) != h["boot_id"].as_str());
                    if let Ok(t) = serde_json::from_value(h["traffic"].clone()) {
                        util::lock(&app.proxy.remote_traffic).insert(n.id.clone(), t);
                    }
                    let mut h = h;
                    if let Some(o) = h.as_object_mut() {
                        o.remove("traffic");
                    }
                    let _ = app.db.conn().execute("UPDATE nodes SET status = 'online', last_seen_at = ?1, info = ?2 WHERE id = ?3", params![now, h.to_string(), n.id]);
                    if was != "online" || rebooted {
                        tracing::info!("server {} is online", n.name);
                        app.emit("nodes", None, None, json!({ "online": n.name }));
                        crate::proxy::reload(&app);
                        push_routes(&app);
                        push_certs(&app).await;
                        // A short blip changes nothing; a long absence or a reboot needs repair.
                        if (was != "unreachable" || rebooted)
                            && let Err(e) = restore(&app, &n).await
                        {
                            tracing::warn!("restoring {}: {e:#}", n.name);
                        }
                    }
                    if let Ok(snap) = agent(&n, reqwest::Method::GET, "/agent/metrics", None).await
                        && let Ok(snap) = serde_json::from_value::<crate::metrics::Snapshot>(snap)
                    {
                        if let Err(e) = crate::metrics::store_for(&app, &n.id, &snap) {
                            tracing::debug!("storing metrics of {}: {e:#}", n.name);
                        }
                        crate::alerts::evaluate(&app, Some(&n), &snap).await;
                        util::lock(&app.metrics.nodes).insert(n.id.clone(), snap);
                    }
                }
                Err(e) => {
                    // Traffic and new replicas avoid it from the first failed check.
                    let dropped = app.db.conn().execute("UPDATE nodes SET status = 'unreachable' WHERE id = ?1 AND status = 'online'", [&n.id]).unwrap_or(0);
                    if dropped > 0 {
                        tracing::info!("server {} stopped answering: {e:#}", n.name);
                        app.emit("nodes", None, None, json!({ "unreachable": n.name }));
                        crate::proxy::reload(&app);
                    }
                    // A server that just joined counts from when it joined.
                    let last: Option<i64> =
                        app.db.conn().query_row("SELECT COALESCE(last_seen_at, created_at) FROM nodes WHERE id = ?1", [&n.id], |r| r.get(0)).optional().ok().flatten();
                    let gone = now - last.unwrap_or(now);
                    if gone > 30 {
                        let changed = app.db.conn().execute("UPDATE nodes SET status = 'offline' WHERE id = ?1 AND status != 'offline'", [&n.id]).unwrap_or(0);
                        if changed > 0 {
                            tracing::warn!("server {} is offline: {e:#}", n.name);
                            app.emit("nodes", None, None, json!({ "offline": n.name }));
                            crate::notify::event(&app, "node.offline", &format!("Server {} is offline", n.name), &format!("{e:#}"), None).await;
                        }
                    }
                    let key = format!("rescheduled:{}", n.id);
                    if gone > RESCHEDULE_AFTER_S && app.db.get_state(&key).ok().flatten().is_none() {
                        let _ = app.db.set_state(&key, &now.to_string());
                        reschedule(&app, &n).await;
                    }
                }
            }
        }
        if ticks.is_multiple_of(30) {
            push_routes(&app);
            push_peers(&app);
            push_certs(&app).await;
        }
    }
}

/// Services that ran on a server that went away start again on the others. Services
/// keeping data there wait for it to come back.
async fn reschedule(app: &AppState, node: &NodeHandle) {
    let Ok(services) = store::services(app, None) else { return };
    for s in services {
        let placed = app.db.get_state(&format!("nodes:{}", s.id)).ok().flatten().unwrap_or_default();
        if s.kind == "compose" || !placed.split(',').any(|id| id == node.id) || stopped(app, &s.id) {
            continue;
        }
        if is_pinned(&s) {
            let msg = format!("{} keeps its data on {} and waits for that server to come back.", s.name, node.name);
            crate::notify::event(app, "node.offline", &format!("{}/{} is down", s.project, s.name), &msg, Some(&s)).await;
            continue;
        }
        let Some(image) = runtime::live_deploy(app, &s.id).and_then(|d| crate::deploy::get(app, &d).ok().flatten()).and_then(|d| d.image) else { continue };
        let req = crate::deploy::Request { trigger: "reschedule".into(), image: Some(image), ..Default::default() };
        match crate::deploy::enqueue(app, &s, req) {
            Ok(_) => tracing::info!("moving {}/{} off {}", s.project, s.name, node.name),
            Err(e) => tracing::error!("moving {}/{} off {}: {e:#}", s.project, s.name, node.name),
        }
    }
}

/// Stopped by someone or asleep: nothing should start it on its own.
pub fn stopped(app: &App, service_id: &str) -> bool {
    app.db.get_state(&format!("stopped:{service_id}")).ok().flatten().is_some() || crate::sleep::is_asleep(app, service_id)
}

/// A server came back, perhaps rebooted, which empties the secrets in its /run: send them
/// again, start what should run, and remove containers that were replaced meanwhile.
async fn restore(app: &AppState, node: &NodeHandle) -> Result<()> {
    let _ = app.db.conn().execute("DELETE FROM state WHERE key = ?1", [format!("rescheduled:{}", node.id)]);
    let services: HashMap<String, Service> = store::services(app, None)?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let filters = HashMap::from([("label".to_string(), vec![format!("{}=true", runtime::L_MANAGED)])]);
    let list = node.docker.list_containers(Some(ListContainersOptions { all: true, filters: Some(filters), ..Default::default() })).await?;
    let mut started = 0;
    for c in list {
        let labels = c.labels.unwrap_or_default();
        let (Some(id), Some(sid)) = (c.id, labels.get(runtime::L_SERVICE)) else { continue };
        let name = c.names.and_then(|n| n.first().cloned()).unwrap_or_default().trim_start_matches('/').to_string();
        let Some(s) = services.get(sid) else {
            runtime::remove(app, &node.id, &id, &name).await?;
            continue;
        };
        if s.kind == "compose" {
            continue;
        }
        let current = app.deployer.busy(&s.id) || runtime::live_deploy(app, &s.id).as_deref() == labels.get(runtime::L_DEPLOY).map(String::as_str);
        if !current {
            runtime::remove(app, &node.id, &id, &name).await?;
            continue;
        }
        put_secrets(app, node, &name, s).await?;
        let running = c.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING);
        if !running && !stopped(app, &s.id) {
            runtime::start(app, &node.id, &id).await?;
            started += 1;
        }
    }
    if started > 0 {
        tracing::info!("started {started} containers again on {}", node.name);
        crate::proxy::reload(app);
    }
    Ok(())
}

pub fn online(app: &App, node_id: &str) -> bool {
    if node_id == "local" {
        return true;
    }
    app.db.conn().query_row("SELECT status = 'online' FROM nodes WHERE id = ?1", [node_id], |r| r.get(0)).unwrap_or(false)
}

/// Remove a server from the mesh and the swarm.
pub async fn remove(app: &AppState, node_id: &str) -> Result<()> {
    let node = app.node(node_id).ok_or_else(|| anyhow!("no such server"))?;
    if node.local {
        bail!("the panel's own server cannot be removed");
    }
    let name = node.name.clone();
    let info: String = app.db.conn().query_row("SELECT info FROM nodes WHERE id = ?1", [node_id], |r| r.get(0)).unwrap_or_default();
    let swarm_node = serde_json::from_str::<Value>(&info).ok().and_then(|i| i["swarm_node"].as_str().map(String::from));
    let _ = agent(&node, reqwest::Method::POST, "/agent/leave", None).await;
    app.db.conn().execute("DELETE FROM nodes WHERE id = ?1", [node_id])?;
    app.db.conn().execute(
        "DELETE FROM state WHERE key IN (?1, ?2, ?3, ?4)",
        params![format!("nodekey:{node_id}"), format!("rescheduled:{node_id}"), format!("draining:{node_id}"), format!("panelep:{node_id}")],
    )?;
    let _ = std::fs::remove_dir_all(app.paths.run.join("docker").join(node_id));
    set_peers(app)?;
    load(app)?;
    push_peers(app);
    if let Some(id) = swarm_node {
        let _ = app.docker.delete_node(&id, Some(bollard::query_parameters::DeleteNodeOptions { force: true })).await;
    }
    tracing::info!("removed server {name}");
    crate::proxy::reload(app);
    Ok(())
}

/// Whether services keep data on this server (it cannot be emptied by moving them).
pub fn keeps_data(app: &App, node_id: &str) -> Result<bool> {
    Ok(app.db.conn().query_row("SELECT COUNT(*) > 0 FROM state WHERE key LIKE 'pin:%' AND value = ?1", [node_id], |r| r.get(0))?)
}

/// Empty a server before removing it: no new replicas there, and the ones it runs move.
pub async fn drain(app: &AppState, node_id: &str) -> Result<()> {
    app.db.set_state(&format!("draining:{node_id}"), &util::now().to_string())?;
    let mut moving = vec![];
    for s in store::services(app, None)? {
        let placed = app.db.get_state(&format!("nodes:{}", s.id))?.unwrap_or_default();
        if s.kind == "compose" || is_pinned(&s) || stopped(app, &s.id) || !placed.split(',').any(|id| id == node_id) {
            continue;
        }
        let Some(image) = runtime::live_deploy(app, &s.id).and_then(|d| crate::deploy::get(app, &d).ok().flatten()).and_then(|d| d.image) else { continue };
        crate::deploy::enqueue(app, &s, crate::deploy::Request { trigger: "drain".into(), image: Some(image), ..Default::default() })?;
        moving.push(s.id);
    }
    let started = std::time::Instant::now();
    while moving.iter().any(|id| app.deployer.busy(id)) {
        if started.elapsed() > Duration::from_secs(1800) {
            bail!("moving services off the server took too long");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

/// Nodes a service may run on, honoring its placement rules.
pub fn eligible(app: &App, placement: &crate::model::Placement) -> Vec<Arc<NodeHandle>> {
    app.all_nodes()
        .into_iter()
        .filter(|n| online(app, &n.id))
        .filter(|n| app.db.get_state(&format!("draining:{}", n.id)).ok().flatten().is_none())
        .filter(|n| placement.node.as_ref().is_none_or(|want| *want == n.id || *want == n.name))
        .filter(|n| placement.region.as_ref().is_none_or(|r| n.region.as_ref() == Some(r)))
        .filter(|n| placement.labels.iter().all(|(k, v)| n.labels.get(k) == Some(v)))
        .collect()
}

/// Services whose data or host ports live on one server stay there.
pub fn is_pinned(s: &Service) -> bool {
    s.kind == "database"
        || s.spec.host_network
        || s.spec.docker_socket
        || !s.spec.ports.is_empty()
        || s.spec.mounts.iter().any(|m| !matches!(m, Mount::File { .. }))
}

/// Servers for `n` more replicas (next to ones running on `existing` servers): pinned
/// services stay where their data is, the others spread out, then go to the least busy.
pub async fn place(app: &App, s: &Service, n: u32, existing: &[String]) -> Result<Vec<Arc<NodeHandle>>> {
    let candidates = eligible(app, &s.spec.placement);
    if candidates.is_empty() {
        bail!("no online server matches this service's placement");
    }
    if is_pinned(s) {
        let key = format!("pin:{}", s.id);
        let pinned = app.db.get_state(&key)?.and_then(|id| app.node(&id));
        let node = match pinned {
            Some(p) if candidates.iter().any(|c| c.id == p.id) => p,
            Some(p) if s.spec.placement == Default::default() => bail!("{} keeps its data on {}, which is offline", s.name, p.name),
            _ => candidates.iter().find(|c| c.local).or(candidates.first()).cloned().ok_or_else(|| anyhow!("no server"))?,
        };
        app.db.set_state(&key, &node.id)?;
        return Ok(vec![node; n as usize]);
    }
    // Spread replicas over as many servers as possible, then prefer the least busy.
    let filters = HashMap::from([("label".to_string(), vec![format!("{}=true", runtime::L_MANAGED)])]);
    let mut slots = vec![];
    for c in candidates {
        let busy = c.docker.list_containers(Some(ListContainersOptions { filters: Some(filters.clone()), ..Default::default() })).await.map(|l| l.len()).unwrap_or(usize::MAX / 2);
        let mine = existing.iter().filter(|id| **id == c.id).count();
        slots.push((c, mine, busy));
    }
    let mut out = vec![];
    for _ in 0..n {
        let Some(best) = slots.iter_mut().min_by_key(|(_, mine, busy)| (*mine, *busy)) else { break };
        best.1 += 1;
        best.2 += 1;
        out.push(best.0.clone());
    }
    Ok(out)
}

/// Make `image` available on a server: pulled there when it comes from a registry,
/// otherwise sent through the panel's registry on the mesh (only missing layers travel).
pub async fn ship_image(app: &App, node: &NodeHandle, s: &Service, image: &str, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    if node.local || node.docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    if matches!(s.spec.source, Source::Image { .. }) {
        log(&format!("Pulling {image} on {}", node.name));
        return runtime::pull_on(app, &node.docker, image, log).await;
    }
    log(&format!("Sending the image to {}", node.name));
    let local = format!("localhost:{REGISTRY_PORT}/{image}");
    let mut tag = tokio::process::Command::new("docker");
    tag.args(["tag", image, &local]);
    crate::proc::output(tag).await?;
    let mut push = tokio::process::Command::new("docker");
    push.args(["push", "--quiet", &local]);
    let pushed = crate::proc::output(push).await;
    // Every server's host can write to the registry, so pull exactly what was pushed: by digest.
    let digest = app.docker.inspect_image(&local).await.ok().and_then(|i| i.repo_digests).unwrap_or_default().into_iter().find_map(|d| {
        let (repo, digest) = d.split_once('@')?;
        (repo == runtime::split_image(&local).0).then(|| digest.to_string())
    });
    let mut untag = tokio::process::Command::new("docker");
    untag.args(["rmi", &local]);
    let _ = crate::proc::output(untag).await;
    pushed.context("sending the image to the mesh registry")?;
    let digest = digest.ok_or_else(|| anyhow!("the registry did not report the image's digest"))?;
    let remote = format!("{}:{REGISTRY_PORT}/{}@{digest}", mesh::PANEL_IP, runtime::split_image(image).0);
    runtime::pull_on(app, &node.docker, &remote, log).await?;
    let (repo, tag) = runtime::split_image(image);
    node.docker
        .tag_image(&remote, Some(bollard::query_parameters::TagImageOptions { repo: Some(repo.into()), tag: Some(tag.into()) }))
        .await
        .with_context(|| format!("tagging the image on {}", node.name))?;
    let _ = node.docker.remove_image(&remote, None::<bollard::query_parameters::RemoveImageOptions>, None).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::join_secret;
    use crate::model::Placement;

    #[test]
    fn broken_swarm_is_reported() {
        use super::swarm_issue;
        use bollard::models::LocalNodeState::*;
        use std::time::Duration;
        let fresh = Duration::ZERO;
        // The panel after a reboot: the manager could not bind its mesh address.
        let e = swarm_issue(Some(ERROR), Some("listen tcp 10.99.0.1:2377: bind: cannot assign requested address"), fresh).unwrap();
        assert!(e.contains("10.99.0.1:2377") && e.contains("systemctl restart docker"));
        assert!(swarm_issue(Some(ERROR), Some(" "), fresh).unwrap().starts_with("Swarm mode failed on this server, so"));
        // A worker joining for the first time is pending for a moment; stuck is reported.
        assert_eq!(swarm_issue(Some(PENDING), None, Duration::from_secs(119)), None);
        let p = swarm_issue(Some(PENDING), None, Duration::from_secs(300)).unwrap();
        assert!(p.contains("for 5 minutes") && p.contains("panel's server is up") && p.contains("systemctl restart docker"));
        for ok in [ACTIVE, INACTIVE, LOCKED, EMPTY] {
            assert_eq!(swarm_issue(Some(ok), None, Duration::from_secs(300)), None);
        }
        assert_eq!(swarm_issue(None, None, fresh), None);
    }

    #[test]
    fn mesh_up_without_a_mesh_does_nothing() {
        let dir = std::env::temp_dir().join(format!("tugport-meshup-{}", std::process::id()));
        let paths = crate::paths::Paths { etc: dir.join("etc"), data: dir.join("data"), run: dir.join("run") };
        std::fs::create_dir_all(&paths.etc).unwrap();
        std::fs::create_dir_all(&paths.data).unwrap();
        // Nothing saved: a fresh or single-server install.
        assert!(!super::mesh_up(&paths).unwrap());
        assert!(!crate::agent::mesh_up(&paths).unwrap());
        // A panel database with the mesh on but no key (it never came up here), then without the mesh.
        let db = crate::db::Db::open(&paths.db()).unwrap();
        db.set_state("mesh", "sealed").unwrap();
        assert!(!super::mesh_up(&paths).unwrap());
        db.conn().execute("DELETE FROM state WHERE key = 'mesh'", []).unwrap();
        drop(db);
        std::fs::write(paths.etc.join("wg.key"), "key").unwrap();
        assert!(!super::mesh_up(&paths).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn endpoints_prefer_what_can_be_reached() {
        use super::reachable_endpoint;
        let ip = |s: &str| Some(s.parse().unwrap());
        // EC2: the machine only knows its private address; we saw its public one.
        assert_eq!(reachable_endpoint(Some("172.31.5.9:51820"), ip("3.120.1.2")).as_deref(), Some("3.120.1.2:51820"));
        // A server with a public address keeps it.
        assert_eq!(reachable_endpoint(Some("95.216.1.2:51820"), ip("95.216.1.2")).as_deref(), Some("95.216.1.2:51820"));
        // Both private (one LAN or one VPC): keep what it said.
        assert_eq!(reachable_endpoint(Some("172.17.0.8:51820"), ip("172.17.0.8")).as_deref(), Some("172.17.0.8:51820"));
        assert_eq!(reachable_endpoint(None, ip("10.0.0.2")), None);
        assert_eq!(reachable_endpoint(None, ip("::ffff:3.120.1.2")).as_deref(), Some("3.120.1.2:51820"));
    }

    #[test]
    fn hostnames_become_server_names() {
        use super::server_name;
        assert_eq!(server_name("Web-01.Example.com").as_deref(), Some("web-01-example-com"));
        assert_eq!(server_name("1box").as_deref(), Some("server-1box"));
        assert_eq!(server_name("--").as_deref(), None);
        assert!(server_name("Web-01.Example.com").is_some_and(|n| crate::util::valid_name(&n)));
    }

    #[test]
    fn joins_never_reveal_the_token() {
        let token = "secret-token";
        let id = hex::encode(join_secret(token, "id").unwrap());
        assert!(!id.contains(token) && id.len() == 64);
        // A proof only fits the WireGuard key it was made for.
        let proof = join_secret(token, "proof:KEY-A").unwrap();
        assert_ne!(proof, join_secret(token, "proof:KEY-B").unwrap());
        assert_ne!(proof, join_secret("other-token", "proof:KEY-A").unwrap());
        // Only the token holder opens the panel's answer.
        let sealed = crate::crypto::Keyring::new(&join_secret(token, "response").unwrap()).seal_str("join", "{}").unwrap();
        assert!(crate::crypto::Keyring::new(&join_secret(token, "response").unwrap()).open_str("join", &sealed).is_ok());
        assert!(crate::crypto::Keyring::new(&join_secret("other-token", "response").unwrap()).open_str("join", &sealed).is_err());
    }

    #[test]
    fn placement_rules_match() {
        let p = Placement { node: None, region: Some("eu".into()), labels: [("gpu".to_string(), "yes".to_string())].into() };
        let labels: std::collections::BTreeMap<String, String> = [("gpu".to_string(), "yes".to_string())].into();
        assert!(p.labels.iter().all(|(k, v)| labels.get(k) == Some(v)));
        assert_eq!(p.region.as_deref(), Some("eu"));
    }
}
