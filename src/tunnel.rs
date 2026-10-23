//! Cloudflare Tunnel: domains marked `via_tunnel` are served through a tunnel that
//! Tugport creates, runs (`cloudflared`) and keeps in sync, so no port needs to be open.

use crate::app::{App, AppState};
use crate::{runtime, store, util};
use anyhow::{Context, Result, anyhow, bail};
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountTypeEnum, RestartPolicy, RestartPolicyNameEnum};
use bollard::query_parameters::{CreateContainerOptions, RemoveContainerOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

const IMAGE: &str = "cloudflare/cloudflared:2026.10.3";
const CONTAINER: &str = "tugport-cloudflared";
/// HTTP/2 rather than QUIC: QUIC needs outbound UDP 7844, which many networks drop,
/// and cloudflared then takes minutes to fall back.
const CMD: [&str; 7] = ["tunnel", "--no-autoupdate", "--protocol", "http2", "run", "--token-file", "/run/tunnel-token"];
const API: &str = "https://api.cloudflare.com/client/v4";

static KICK: tokio::sync::Notify = tokio::sync::Notify::const_new();

#[derive(Serialize, Deserialize, Clone)]
pub struct Tunnel {
    pub api_token: String,
    pub account_id: String,
    pub tunnel_id: String,
    pub tunnel_token: String,
}

pub fn load(app: &App) -> Result<Option<Tunnel>> {
    match app.db.get_state("tunnel")? {
        Some(sealed) => Ok(Some(serde_json::from_str(&app.keys.open_str("tunnel", &crate::crypto::unb64(&sealed)?)?)?)),
        None => Ok(None),
    }
}

fn save(app: &App, t: &Tunnel) -> Result<()> {
    let sealed = app.keys.seal_str("tunnel", &serde_json::to_string(t)?)?;
    app.db.set_state("tunnel", &crate::crypto::b64(&sealed))
}

/// Re-sync tunnel ingress rules and DNS records in the background.
pub fn sync_soon(app: &AppState) {
    let _ = app;
    KICK.notify_one();
}

pub async fn run(app: AppState) {
    loop {
        tokio::select! {
            _ = KICK.notified() => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {}
        }
        match load(&app) {
            Ok(Some(t)) => {
                if let Err(e) = ensure_running(&app, &t).await {
                    tracing::warn!("cloudflared: {e:#}");
                }
                if let Err(e) = sync(&app, &t).await {
                    tracing::warn!("tunnel sync: {e:#}");
                    app.emit("warning", None, None, json!({ "message": format!("Cloudflare tunnel sync failed: {e:#}") }));
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("reading tunnel settings: {e:#}"),
        }
    }
}

fn client(token: &str) -> Result<(reqwest::Client, String)> {
    Ok((reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?, format!("Bearer {token}")))
}

async fn cf(token: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
    let (c, auth) = client(token)?;
    let mut req = c.request(method, format!("{API}{path}")).header("Authorization", auth);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let v: Value = req.send().await?.json().await?;
    if v["success"].as_bool() != Some(true) {
        let msg = v["errors"].as_array().and_then(|e| e.first()).and_then(|e| e["message"].as_str()).unwrap_or("request failed");
        bail!("Cloudflare: {msg}");
    }
    Ok(v["result"].clone())
}

/// Create the tunnel with a user's API token (needs Account: Cloudflare Tunnel Edit and Zone: DNS Edit).
pub async fn connect(app: &AppState, api_token: &str) -> Result<Tunnel> {
    let accounts = cf(api_token, reqwest::Method::GET, "/accounts?per_page=5", None).await.context("the token cannot list accounts")?;
    let account_id = accounts.as_array().and_then(|a| a.first()).and_then(|a| a["id"].as_str()).ok_or_else(|| anyhow!("the token sees no Cloudflare account"))?.to_string();
    let host: String = app.db.conn().query_row("SELECT name FROM nodes WHERE id = 'local'", [], |r| r.get(0)).unwrap_or_else(|_| "server".into());
    let name = format!("tugport-{}-{}", host.replace(|c: char| !c.is_ascii_alphanumeric(), "-"), &util::id()[..4]);
    let created = cf(api_token, reqwest::Method::POST, &format!("/accounts/{account_id}/cfd_tunnel"), Some(json!({ "name": name, "config_src": "cloudflare" })))
        .await
        .context("creating the tunnel (the token needs Account > Cloudflare Tunnel > Edit)")?;
    let tunnel_id = created["id"].as_str().ok_or_else(|| anyhow!("Cloudflare returned no tunnel id"))?.to_string();
    let token = cf(api_token, reqwest::Method::GET, &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/token"), None).await?;
    let tunnel_token = token.as_str().ok_or_else(|| anyhow!("Cloudflare returned no tunnel token"))?.to_string();
    let t = Tunnel { api_token: api_token.to_string(), account_id, tunnel_id, tunnel_token };
    save(app, &t)?;
    let up = async {
        ensure_running(app, &t).await?;
        sync(app, &t).await
    }
    .await;
    if let Err(e) = up {
        // A tunnel that can't come up is removed again, so a failed connect leaves nothing at Cloudflare.
        let _ = disconnect(app).await;
        return Err(e);
    }
    Ok(t)
}

/// Stop cloudflared and remove what Tugport made at Cloudflare: its DNS records and the tunnel.
pub async fn disconnect(app: &App) -> Result<()> {
    let _ = app.docker.remove_container(CONTAINER, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
    let mut left = Vec::new();
    if let Some(t) = load(app)? {
        for (host, rec) in dns_records(app) {
            if let Err(e) = remove_record(&t, &rec).await {
                left.push(format!("the DNS record for {host} ({e:#})"));
            }
        }
        // Cloudflare keeps a stopped connector's connections for a moment; clear them, then delete.
        let path = format!("/accounts/{}/cfd_tunnel/{}", t.account_id, t.tunnel_id);
        let mut last = None;
        for _ in 0..10 {
            let _ = cf(&t.api_token, reqwest::Method::DELETE, &format!("{path}/connections"), None).await;
            match cf(&t.api_token, reqwest::Method::DELETE, &path, None).await {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        if let Some(e) = last {
            left.push(format!("tunnel {} ({e:#})", t.tunnel_id));
        }
    }
    app.db.conn().execute("DELETE FROM state WHERE key IN ('tunnel', 'tunnel_dns')", [])?;
    let _ = std::fs::remove_file(token_path(app));
    if !left.is_empty() {
        bail!("the tunnel is disconnected, but remove these in the Cloudflare dashboard: {}", left.join("; "));
    }
    Ok(())
}

/// A DNS record Tugport made for a tunnel domain.
#[derive(Serialize, Deserialize, Clone)]
struct DnsRecord {
    zone: String,
    id: String,
}

fn dns_records(app: &App) -> std::collections::BTreeMap<String, DnsRecord> {
    app.db.get_state("tunnel_dns").ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// Delete a record Tugport made, unless it is already gone or someone has pointed it elsewhere since.
async fn remove_record(t: &Tunnel, rec: &DnsRecord) -> Result<()> {
    let path = format!("/zones/{}/dns_records/{}", rec.zone, rec.id);
    let Ok(r) = cf(&t.api_token, reqwest::Method::GET, &path, None).await else { return Ok(()) };
    if r["content"].as_str() != Some(format!("{}.cfargotunnel.com", t.tunnel_id).as_str()) {
        return Ok(());
    }
    cf(&t.api_token, reqwest::Method::DELETE, &path, None).await.map(|_| ())
}

/// In the root-only secrets folder: anyone holding the token can run a connector for the tunnel.
fn token_path(app: &App) -> std::path::PathBuf {
    app.paths.secrets().join("cloudflared.token")
}

pub async fn status(app: &App) -> Result<Value> {
    let Some(t) = load(app)? else { return Ok(json!({ "enabled": false })) };
    let running = app
        .docker
        .inspect_container(CONTAINER, None::<bollard::query_parameters::InspectContainerOptions>)
        .await
        .ok()
        .and_then(|i| i.state)
        .and_then(|s| s.running)
        .unwrap_or(false);
    let domains: Vec<String> = store::domains(app, None)?.into_iter().filter(|d| d.via_tunnel).map(|d| d.host).collect();
    Ok(json!({ "enabled": true, "account": t.account_id, "tunnel_id": t.tunnel_id, "running": running, "domains": domains }))
}

async fn ensure_running(app: &App, t: &Tunnel) -> Result<()> {
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o444).open(token_path(app))?;
        f.write_all(t.tunnel_token.as_bytes())?;
    }
    if app.docker.inspect_image(IMAGE).await.is_err() {
        runtime::pull(app, IMAGE, &|_| {}).await?;
    }
    let cmd: Vec<String> = CMD.map(String::from).to_vec();
    if let Ok(i) = app.docker.inspect_container(CONTAINER, None::<bollard::query_parameters::InspectContainerOptions>).await {
        // A container from an older Tugport (another image or command) is replaced.
        let current = i.config.is_some_and(|c| c.image.as_deref() == Some(IMAGE) && c.cmd.as_ref() == Some(&cmd));
        if current && i.state.and_then(|s| s.running).unwrap_or(false) {
            return Ok(());
        }
        let _ = app.docker.remove_container(CONTAINER, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
    }
    let labels = HashMap::from([(runtime::L_MANAGED.to_string(), "true".to_string()), ("tugport.system".to_string(), "tunnel".to_string())]);
    let config = ContainerCreateBody {
        image: Some(IMAGE.into()),
        cmd: Some(cmd),
        labels: Some(labels),
        host_config: Some(HostConfig {
            network_mode: Some("host".into()),
            restart_policy: Some(RestartPolicy { name: Some(RestartPolicyNameEnum::UNLESS_STOPPED), maximum_retry_count: None }),
            mounts: Some(vec![Mount {
                typ: Some(MountTypeEnum::BIND),
                source: Some(token_path(app).to_string_lossy().to_string()),
                target: Some("/run/tunnel-token".into()),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    app.docker.create_container(Some(CreateContainerOptions { name: Some(CONTAINER.into()), ..Default::default() }), config).await?;
    runtime::start(app, "local", CONTAINER).await
}

/// Point every tunnel domain at our proxy, and make each hostname's DNS a proxied CNAME to the tunnel.
async fn sync(app: &App, t: &Tunnel) -> Result<()> {
    let http_port = std::env::var("TUGPORT_HTTP_PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(80);
    let mut hosts: Vec<String> = store::domains(app, None)?.into_iter().filter(|d| d.via_tunnel).map(|d| d.host).collect();
    hosts.sort();
    hosts.dedup();
    let mut ingress: Vec<Value> = hosts.iter().map(|h| json!({ "hostname": h, "service": format!("http://127.0.0.1:{http_port}") })).collect();
    ingress.push(json!({ "service": "http_status:404" }));
    cf(
        &t.api_token,
        reqwest::Method::PUT,
        &format!("/accounts/{}/cfd_tunnel/{}/configurations", t.account_id, t.tunnel_id),
        Some(json!({ "config": { "ingress": ingress } })),
    )
    .await
    .context("updating tunnel routes")?;
    let target = format!("{}.cfargotunnel.com", t.tunnel_id);
    let mut ours = dns_records(app);
    // One domain's problem doesn't hold up the others.
    let mut failed = Vec::new();
    for host in &hosts {
        match point_at(&t.api_token, host, &target).await {
            Ok(rec) => {
                ours.insert(host.clone(), rec);
            }
            Err(e) => failed.push(format!("{host}: {e:#}")),
        }
    }
    // Names that left the tunnel lose the records Tugport made for them.
    let gone: Vec<String> = ours.keys().filter(|h| !hosts.contains(h)).cloned().collect();
    for host in gone {
        if let Some(rec) = ours.remove(&host)
            && let Err(e) = remove_record(t, &rec).await
        {
            failed.push(format!("removing the DNS record for {host}: {e:#}"));
            ours.insert(host, rec);
        }
    }
    app.db.set_state("tunnel_dns", &serde_json::to_string(&ours)?)?;
    if !failed.is_empty() {
        bail!("{}", failed.join("; "));
    }
    Ok(())
}

/// Make `host` a proxied CNAME to the tunnel, returning the record.
async fn point_at(token: &str, host: &str, target: &str) -> Result<DnsRecord> {
    let zone = zone_for(token, host).await?;
    let records = cf(token, reqwest::Method::GET, &format!("/zones/{zone}/dns_records?name={host}"), None).await?;
    let existing = records.as_array().cloned().unwrap_or_default();
    let body = json!({ "type": "CNAME", "name": host, "content": target, "proxied": true, "comment": "managed by Tugport" });
    let id = match existing.iter().find(|r| r["type"] == "CNAME") {
        Some(r) if r["content"].as_str() == Some(target) => r["id"].as_str().unwrap_or_default().to_string(),
        // Someone else's record (a SaaS CNAME, say) is left alone, like A/AAAA records.
        Some(r) if r["comment"].as_str() != Some("managed by Tugport") => {
            bail!("it has a CNAME record to {}; remove it so the tunnel can take over the name", r["content"].as_str().unwrap_or("somewhere else"));
        }
        Some(r) => {
            let id = r["id"].as_str().unwrap_or_default();
            cf(token, reqwest::Method::PUT, &format!("/zones/{zone}/dns_records/{id}"), Some(body)).await?;
            id.to_string()
        }
        None if existing.iter().any(|r| r["type"] == "A" || r["type"] == "AAAA") => {
            bail!("it has A/AAAA records; remove them so the tunnel can take over the name");
        }
        None => cf(token, reqwest::Method::POST, &format!("/zones/{zone}/dns_records"), Some(body)).await?["id"].as_str().unwrap_or_default().to_string(),
    };
    Ok(DnsRecord { zone, id })
}

async fn zone_for(token: &str, host: &str) -> Result<String> {
    let labels: Vec<&str> = host.split('.').collect();
    for i in 0..labels.len().saturating_sub(1) {
        let name = labels[i..].join(".");
        let zones = cf(token, reqwest::Method::GET, &format!("/zones?name={name}"), None).await?;
        if let Some(id) = zones.as_array().and_then(|z| z.first()).and_then(|z| z["id"].as_str()) {
            return Ok(id.to_string());
        }
    }
    bail!("no Cloudflare zone in this account covers {host}")
}
