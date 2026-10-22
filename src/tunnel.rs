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
    let host = crate::public_ip().unwrap_or_else(|| "server".into());
    let name = format!("tugport-{}-{}", host.replace(['.', ':'], "-"), &util::id()[..4]);
    let created = cf(api_token, reqwest::Method::POST, &format!("/accounts/{account_id}/cfd_tunnel"), Some(json!({ "name": name, "config_src": "cloudflare" })))
        .await
        .context("creating the tunnel")?;
    let tunnel_id = created["id"].as_str().ok_or_else(|| anyhow!("Cloudflare returned no tunnel id"))?.to_string();
    let token = cf(api_token, reqwest::Method::GET, &format!("/accounts/{account_id}/cfd_tunnel/{tunnel_id}/token"), None).await?;
    let tunnel_token = token.as_str().ok_or_else(|| anyhow!("Cloudflare returned no tunnel token"))?.to_string();
    let t = Tunnel { api_token: api_token.to_string(), account_id, tunnel_id, tunnel_token };
    save(app, &t)?;
    ensure_running(app, &t).await?;
    sync(app, &t).await?;
    Ok(t)
}

pub async fn disconnect(app: &App) -> Result<()> {
    let _ = app.docker.remove_container(CONTAINER, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
    if let Some(t) = load(app)? {
        let _ = cf(&t.api_token, reqwest::Method::DELETE, &format!("/accounts/{}/cfd_tunnel/{}", t.account_id, t.tunnel_id), None).await;
    }
    app.db.conn().execute("DELETE FROM state WHERE key = 'tunnel'", [])?;
    let _ = std::fs::remove_file(token_path(app));
    Ok(())
}

fn token_path(app: &App) -> std::path::PathBuf {
    app.paths.run.join("cloudflared.token")
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
    if let Ok(i) = app.docker.inspect_container(CONTAINER, None::<bollard::query_parameters::InspectContainerOptions>).await {
        if i.state.and_then(|s| s.running).unwrap_or(false) {
            return Ok(());
        }
        let _ = app.docker.remove_container(CONTAINER, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
    }
    if app.docker.inspect_image(IMAGE).await.is_err() {
        runtime::pull(app, IMAGE, &|_| {}).await?;
    }
    let labels = HashMap::from([(runtime::L_MANAGED.to_string(), "true".to_string()), ("tugport.system".to_string(), "tunnel".to_string())]);
    let config = ContainerCreateBody {
        image: Some(IMAGE.into()),
        cmd: Some(vec!["tunnel".into(), "--no-autoupdate".into(), "run".into(), "--token-file".into(), "/run/tunnel-token".into()]),
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
    for host in hosts {
        let zone = zone_for(&t.api_token, &host).await?;
        let records = cf(&t.api_token, reqwest::Method::GET, &format!("/zones/{zone}/dns_records?name={host}"), None).await?;
        let existing = records.as_array().cloned().unwrap_or_default();
        let body = json!({ "type": "CNAME", "name": host, "content": target, "proxied": true, "comment": "managed by Tugport" });
        match existing.iter().find(|r| r["type"] == "CNAME") {
            Some(r) if r["content"].as_str() == Some(target.as_str()) => {}
            Some(r) => {
                let id = r["id"].as_str().unwrap_or_default();
                cf(&t.api_token, reqwest::Method::PUT, &format!("/zones/{zone}/dns_records/{id}"), Some(body)).await?;
            }
            None if existing.iter().any(|r| r["type"] == "A" || r["type"] == "AAAA") => {
                bail!("{host} has A/AAAA records; remove them so the tunnel can take over the name");
            }
            None => {
                cf(&t.api_token, reqwest::Method::POST, &format!("/zones/{zone}/dns_records"), Some(body)).await?;
            }
        }
    }
    Ok(())
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
