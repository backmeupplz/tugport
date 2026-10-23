//! Cloud servers: Hetzner and AWS accounts, machines created there that join the mesh by
//! themselves, and pools that add machines under load and remove idle ones.

use crate::app::{App, AppState};
use crate::util;
use anyhow::{Context, Result, anyhow, bail};
use hmac::{Hmac, Mac};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// A machine that has not joined after this long is deleted (it would only cost money).
const JOIN_DEADLINE_S: i64 = 20 * 60;
/// Load must stay low this long before a pool shrinks.
const SHRINK_AFTER_S: i64 = 30 * 60;
/// Time between a pool growing and growing again.
const GROW_COOLDOWN_S: i64 = 10 * 60;

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Credentials {
    Hetzner { token: String },
    Aws { access_key: String, secret_key: String },
}

pub struct Account {
    pub id: String,
    pub name: String,
    pub creds: Credentials,
}

pub fn account(app: &App, id: &str) -> Result<Account> {
    let (name, sealed): (String, Vec<u8>) = app
        .db
        .conn()
        .query_row("SELECT name, config FROM cloud_accounts WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| anyhow!("no such cloud account"))?;
    let creds = serde_json::from_str(&app.keys.open_str(&format!("cloud:{id}"), &sealed)?)?;
    Ok(Account { id: id.into(), name, creds })
}

/// Check the credentials work, then keep them sealed.
pub async fn add_account(app: &App, name: &str, creds: Credentials) -> Result<String> {
    match &creds {
        Credentials::Hetzner { token } => {
            hetzner(token, reqwest::Method::GET, "/locations", None).await.context("checking the Hetzner token")?;
        }
        Credentials::Aws { .. } => {
            aws_query(&creds, "us-east-1", &[("Action", "DescribeRegions")]).await.context("checking the AWS keys")?;
        }
    }
    let id = util::id();
    let kind = match creds {
        Credentials::Hetzner { .. } => "hetzner",
        Credentials::Aws { .. } => "aws",
    };
    let sealed = app.keys.seal_str(&format!("cloud:{id}"), &serde_json::to_string(&creds)?)?;
    app.db.conn().execute("INSERT INTO cloud_accounts (id, kind, name, config, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, kind, name, sealed, util::now()])?;
    Ok(id)
}

/// Regions and machine sizes an account offers.
pub async fn options(acc: &Account) -> Result<Value> {
    match &acc.creds {
        Credentials::Hetzner { token } => {
            let locations = hetzner(token, reqwest::Method::GET, "/locations", None).await?;
            let types = hetzner(token, reqwest::Method::GET, "/server_types?per_page=50", None).await?;
            let regions: Vec<Value> = locations["locations"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| json!({ "id": l["name"], "name": format!("{} ({})", l["city"].as_str().unwrap_or(""), l["country"].as_str().unwrap_or("")) }))
                .collect();
            let sizes: Vec<Value> = types["server_types"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| t["deprecation"].is_null() && !t["deprecated"].as_bool().unwrap_or(false))
                .map(|t| {
                    let hourly = |p: &Value| p["price_hourly"]["gross"].as_str().and_then(|v| v.parse::<f64>().ok());
                    let prices: Vec<Value> = t["prices"].as_array().into_iter().flatten().map(|p| json!({ "region": p["location"], "hourly": hourly(p), "currency": "EUR" })).collect();
                    json!({ "id": t["name"], "cpus": t["cores"], "memory_gb": t["memory"], "disk_gb": t["disk"], "arch": t["architecture"], "prices": prices })
                })
                .collect();
            Ok(json!({ "regions": regions, "sizes": sizes }))
        }
        Credentials::Aws { .. } => {
            let regions: Vec<Value> = AWS_REGIONS.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect();
            let sizes: Vec<Value> = AWS_SIZES.iter().map(|(id, cpus, mem)| json!({ "id": id, "cpus": cpus, "memory_gb": mem, "arch": if aws_arm(id) { "arm" } else { "x86" } })).collect();
            Ok(json!({ "regions": regions, "sizes": sizes }))
        }
    }
}

const AWS_REGIONS: &[(&str, &str)] = &[
    ("us-east-1", "N. Virginia"),
    ("us-east-2", "Ohio"),
    ("us-west-1", "N. California"),
    ("us-west-2", "Oregon"),
    ("ca-central-1", "Canada"),
    ("sa-east-1", "São Paulo"),
    ("eu-west-1", "Ireland"),
    ("eu-west-2", "London"),
    ("eu-west-3", "Paris"),
    ("eu-central-1", "Frankfurt"),
    ("eu-north-1", "Stockholm"),
    ("eu-south-1", "Milan"),
    ("ap-south-1", "Mumbai"),
    ("ap-southeast-1", "Singapore"),
    ("ap-southeast-2", "Sydney"),
    ("ap-northeast-1", "Tokyo"),
    ("ap-northeast-2", "Seoul"),
];

const AWS_SIZES: &[(&str, u32, u32)] = &[
    ("t3.small", 2, 2),
    ("t3.medium", 2, 4),
    ("t3.large", 2, 8),
    ("m7i.large", 2, 8),
    ("m7i.xlarge", 4, 16),
    ("c7i.large", 2, 4),
    ("c7i.xlarge", 4, 8),
    ("t4g.small", 2, 2),
    ("t4g.medium", 2, 4),
    ("m7g.large", 2, 8),
    ("m7g.xlarge", 4, 16),
];

/// Graviton families end in g (t4g, m7g) or gd/gn.
fn aws_arm(size: &str) -> bool {
    let family = size.split('.').next().unwrap_or("");
    family.ends_with('g') || family.ends_with("gd") || family.ends_with("gn")
}

/// Create a machine that installs Tugport and joins this panel on first boot.
pub async fn create_server(app: &AppState, account_id: &str, region: &str, size: &str, pool: Option<&str>, panel_url: &str) -> Result<String> {
    let acc = account(app, account_id)?;
    let id = util::id();
    let name = format!("tugport-{}-{}", region.to_lowercase().replace('_', "-"), &id[..id.len().min(6)]);
    app.db.conn().execute(
        "INSERT INTO cloud_servers (id, account_id, pool_id, name, region, size, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'creating', ?7)",
        params![id, acc.id, pool, name, region, size, util::now()],
    )?;
    let created = async {
        let join = crate::nodes::join_token_for(app, panel_url, Some(&id)).await?;
        let script = format!("#!/bin/sh\n{}\n", join.command);
        match &acc.creds {
            Credentials::Hetzner { token } => {
                let body = json!({
                    "name": name, "server_type": size, "location": region, "image": "ubuntu-24.04",
                    "user_data": script, "labels": { "tugport": "true" }, "start_after_create": true,
                });
                let v = hetzner(token, reqwest::Method::POST, "/servers", Some(body)).await?;
                v["server"]["id"].as_i64().map(|i| i.to_string()).ok_or_else(|| anyhow!("Hetzner did not return a server id"))
            }
            Credentials::Aws { .. } => aws_run_instance(&acc.creds, region, size, &name, &script).await,
        }
    };
    match created.await {
        Ok(cloud_id) => {
            app.db.conn().execute("UPDATE cloud_servers SET cloud_id = ?1 WHERE id = ?2", params![cloud_id, id])?;
            tracing::info!("created {name} ({size} in {region}) at {}", acc.name);
            app.emit("nodes", None, None, json!({ "creating": name }));
            Ok(id)
        }
        Err(e) => {
            app.db.conn().execute("UPDATE cloud_servers SET status = 'failed', error = ?1 WHERE id = ?2", params![format!("{e:#}"), id])?;
            app.emit("nodes", None, None, json!({ "failed": name }));
            Err(e)
        }
    }
}

/// Delete a machine at its provider and forget it.
pub async fn destroy(app: &App, cloud_server_id: &str) -> Result<()> {
    let (account_id, cloud_id, region): (String, Option<String>, String) = app
        .db
        .conn()
        .query_row("SELECT account_id, cloud_id, region FROM cloud_servers WHERE id = ?1", [cloud_server_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    if let Some(cloud_id) = cloud_id {
        let acc = account(app, &account_id)?;
        match &acc.creds {
            Credentials::Hetzner { token } => match hetzner(token, reqwest::Method::DELETE, &format!("/servers/{cloud_id}"), None).await {
                Ok(_) => {}
                Err(e) if format!("{e:#}").contains("not found") => {}
                Err(e) => return Err(e),
            },
            Credentials::Aws { .. } => {
                aws_query(&acc.creds, &region, &[("Action", "TerminateInstances"), ("InstanceId.1", &cloud_id)]).await?;
            }
        }
    }
    app.db.conn().execute("DELETE FROM cloud_servers WHERE id = ?1", [cloud_server_id])?;
    Ok(())
}

/// The cloud machine behind a server, if Tugport created it.
pub fn machine_of(app: &App, node_id: &str) -> Option<String> {
    app.db.conn().query_row("SELECT id FROM cloud_servers WHERE node_id = ?1", [node_id], |r| r.get(0)).optional().ok().flatten()
}

/// Where new servers reach the panel: its domain, or this server's address.
pub fn panel_url(app: &App) -> String {
    let s = app.settings();
    match s.panel_domain {
        Some(d) => format!("https://{d}"),
        None => {
            let port = s.panel_listen.rsplit(':').next().unwrap_or("8484");
            format!("http://{}:{port}", crate::public_ip().unwrap_or_else(|| "127.0.0.1".into()))
        }
    }
}

/// Every minute: delete machines that never joined, keep pools at their size.
pub async fn run(app: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        if let Err(e) = tick(&app).await {
            tracing::warn!("cloud servers: {e:#}");
        }
    }
}

async fn tick(app: &AppState) -> Result<()> {
    let stuck: Vec<(String, String)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT id, name FROM cloud_servers WHERE status = 'creating' AND created_at < ?1")?;
        stmt.query_map([util::now() - JOIN_DEADLINE_S], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (id, name) in stuck {
        tracing::warn!("{name} did not join within {} minutes; deleting it", JOIN_DEADLINE_S / 60);
        let msg = format!("{name} was created but never joined, so Tugport deleted it. Check that it can reach the panel at {}.", panel_url(app));
        crate::notify::event(app, "node.failed", &format!("Server {name} did not join"), &msg, None).await;
        if let Err(e) = destroy(app, &id).await {
            app.db.conn().execute("UPDATE cloud_servers SET status = 'failed', error = ?1 WHERE id = ?2", params![format!("{e:#}"), id])?;
        }
        app.emit("nodes", None, None, json!({ "deleted": name }));
    }
    for pool in pools(app)? {
        if let Err(e) = keep_pool(app, &pool).await {
            tracing::warn!("server pool {}: {e:#}", pool.name);
        }
    }
    Ok(())
}

#[derive(Serialize, Clone)]
pub struct Pool {
    pub id: String,
    pub name: String,
    pub account_id: String,
    pub region: String,
    pub size: String,
    pub labels: Value,
    pub min_servers: u32,
    pub max_servers: u32,
    pub scale_up_cpu: f64,
    pub scale_down_cpu: f64,
}

pub fn pools(app: &App) -> Result<Vec<Pool>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, name, account_id, region, size, labels, min_servers, max_servers, scale_up_cpu, scale_down_cpu FROM server_pools ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok(Pool {
            id: r.get(0)?,
            name: r.get(1)?,
            account_id: r.get(2)?,
            region: r.get(3)?,
            size: r.get(4)?,
            labels: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            min_servers: r.get(6)?,
            max_servers: r.get(7)?,
            scale_up_cpu: r.get(8)?,
            scale_down_cpu: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// What a pool should do next, given its machines and the cluster's CPU.
#[derive(Debug, PartialEq)]
pub enum PoolStep {
    Wait,
    Grow,
    Shrink,
}

pub fn pool_step(p: &Pool, machines: u32, creating: bool, cpu: Option<f64>, low_for_s: i64, since_growth_s: i64) -> PoolStep {
    if creating {
        PoolStep::Wait
    } else if machines < p.min_servers || (machines < p.max_servers && cpu.is_some_and(|c| c > p.scale_up_cpu) && since_growth_s >= GROW_COOLDOWN_S) {
        PoolStep::Grow
    } else if machines > p.min_servers && (machines > p.max_servers || low_for_s >= SHRINK_AFTER_S) {
        PoolStep::Shrink
    } else {
        PoolStep::Wait
    }
}

async fn keep_pool(app: &AppState, p: &Pool) -> Result<()> {
    let members: Vec<(String, String, Option<String>)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT id, status, node_id FROM cloud_servers WHERE pool_id = ?1 AND status IN ('creating', 'joined') ORDER BY created_at")?;
        stmt.query_map([&p.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?
    };
    let creating = members.iter().any(|(_, status, _)| status == "creating");
    let cpu = crate::metrics::cluster_cpu(app, 300);
    let now = util::now();
    let low_key = format!("poollow:{}", p.id);
    let low_since: i64 = match cpu {
        Some(c) if c < p.scale_down_cpu => match app.db.get_state(&low_key)?.and_then(|v| v.parse().ok()) {
            Some(t) => t,
            None => {
                app.db.set_state(&low_key, &now.to_string())?;
                now
            }
        },
        _ => {
            app.db.conn().execute("DELETE FROM state WHERE key = ?1", [&low_key])?;
            now
        }
    };
    let grown: i64 = app.db.get_state(&format!("poolgrew:{}", p.id))?.and_then(|v| v.parse().ok()).unwrap_or(0);
    match pool_step(p, members.len() as u32, creating, cpu, now - low_since, now - grown) {
        PoolStep::Wait => Ok(()),
        PoolStep::Grow => {
            tracing::info!("server pool {} adds a server (CPU {:.0}%)", p.name, cpu.unwrap_or(0.0));
            app.db.set_state(&format!("poolgrew:{}", p.id), &now.to_string())?;
            create_server(app, &p.account_id, &p.region, &p.size, Some(&p.id), &panel_url(app)).await.map(|_| ())
        }
        PoolStep::Shrink => {
            // The newest server that keeps no data goes first.
            for (id, _, node) in members.iter().rev() {
                let Some(node) = node else { continue };
                if crate::nodes::keeps_data(app, node)? {
                    continue;
                }
                tracing::info!("server pool {} removes a server (CPU {:.0}%)", p.name, cpu.unwrap_or(0.0));
                app.db.conn().execute("DELETE FROM state WHERE key = ?1", [&low_key])?;
                crate::nodes::drain(app, node).await?;
                crate::nodes::remove(app, node).await?;
                return destroy(app, id).await;
            }
            Ok(())
        }
    }
}

async fn hetzner(token: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let mut req = client.request(method, format!("https://api.hetzner.cloud/v1{path}")).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.context("reaching Hetzner")?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("Hetzner: {}", v["error"]["message"].as_str().unwrap_or(status.as_str()));
    }
    Ok(v)
}

/// RunInstances with Ubuntu 24.04, a 30 GB disk and the `tugport` security group.
async fn aws_run_instance(creds: &Credentials, region: &str, size: &str, name: &str, script: &str) -> Result<String> {
    let arch = if aws_arm(size) { "arm64" } else { "amd64" };
    let param = format!("/aws/service/canonical/ubuntu/server/24.04/stable/current/{arch}/hvm/ebs-gp3/ami-id");
    let ami: Value = serde_json::from_str(&aws_json(creds, region, "ssm", "AmazonSSM.GetParameter", &json!({ "Name": param })).await?)?;
    let ami = ami["Parameter"]["Value"].as_str().ok_or_else(|| anyhow!("no Ubuntu image for {arch} in {region}"))?.to_string();
    let group = aws_security_group(creds, region).await?;
    let user_data = crate::crypto::b64(script.as_bytes());
    let xml = aws_query(
        creds,
        region,
        &[
            ("Action", "RunInstances"),
            ("ImageId", &ami),
            ("InstanceType", size),
            ("MinCount", "1"),
            ("MaxCount", "1"),
            ("UserData", &user_data),
            ("SecurityGroupId.1", &group),
            ("BlockDeviceMapping.1.DeviceName", "/dev/sda1"),
            ("BlockDeviceMapping.1.Ebs.VolumeSize", "30"),
            ("BlockDeviceMapping.1.Ebs.VolumeType", "gp3"),
            ("TagSpecification.1.ResourceType", "instance"),
            ("TagSpecification.1.Tag.1.Key", "Name"),
            ("TagSpecification.1.Tag.1.Value", name),
            ("TagSpecification.1.Tag.2.Key", "tugport"),
            ("TagSpecification.1.Tag.2.Value", "true"),
        ],
    )
    .await?;
    xml_tag(&xml, "instanceId").ok_or_else(|| anyhow!("AWS did not return an instance id"))
}

/// The `tugport` security group in the default VPC: WireGuard between servers, web traffic.
async fn aws_security_group(creds: &Credentials, region: &str) -> Result<String> {
    let found = aws_query(creds, region, &[("Action", "DescribeSecurityGroups"), ("Filter.1.Name", "group-name"), ("Filter.1.Value.1", "tugport")]).await?;
    if let Some(id) = xml_tag(&found, "groupId") {
        return Ok(id);
    }
    let created = aws_query(creds, region, &[("Action", "CreateSecurityGroup"), ("GroupName", "tugport"), ("GroupDescription", "Tugport servers")])
        .await
        .context("creating the tugport security group (the account needs a default VPC)")?;
    let id = xml_tag(&created, "groupId").ok_or_else(|| anyhow!("AWS did not return a security group id"))?;
    let mut rules: Vec<(String, String)> = vec![("Action".into(), "AuthorizeSecurityGroupIngress".into()), ("GroupId".into(), id.clone())];
    for (i, (proto, port)) in [("udp", "51820"), ("tcp", "80"), ("tcp", "443")].iter().enumerate() {
        let n = i + 1;
        rules.push((format!("IpPermissions.{n}.IpProtocol"), proto.to_string()));
        rules.push((format!("IpPermissions.{n}.FromPort"), port.to_string()));
        rules.push((format!("IpPermissions.{n}.ToPort"), port.to_string()));
        rules.push((format!("IpPermissions.{n}.IpRanges.1.CidrIp"), "0.0.0.0/0".into()));
    }
    let rules: Vec<(&str, &str)> = rules.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    aws_query(creds, region, &rules).await?;
    Ok(id)
}

/// An EC2 Query API call (form-encoded, signed with SigV4); returns the XML response.
async fn aws_query(creds: &Credentials, region: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut all: Vec<(&str, &str)> = params.to_vec();
    all.push(("Version", "2016-11-15"));
    let body = all.iter().map(|(k, v)| format!("{}={}", aws_encode(k), aws_encode(v))).collect::<Vec<_>>().join("&");
    aws_call(creds, region, "ec2", "application/x-www-form-urlencoded; charset=utf-8", None, body).await
}

/// A JSON API call (SSM and friends).
async fn aws_json(creds: &Credentials, region: &str, service: &str, target: &str, body: &Value) -> Result<String> {
    aws_call(creds, region, service, "application/x-amz-json-1.1", Some(target), body.to_string()).await
}

async fn aws_call(creds: &Credentials, region: &str, service: &str, content_type: &str, target: Option<&str>, body: String) -> Result<String> {
    let Credentials::Aws { access_key, secret_key } = creds else { bail!("not an AWS account") };
    let host = format!("{service}.{region}.amazonaws.com");
    let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut headers = vec![("content-type", content_type.to_string()), ("host", host.clone()), ("x-amz-date", date.clone())];
    if let Some(t) = target {
        headers.push(("x-amz-target", t.to_string()));
    }
    let auth = sigv4(&Request { method: "POST", path: "/", query: "", headers: &headers, body: body.as_bytes() }, access_key, secret_key, region, service, &date)?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let mut req = client.post(format!("https://{host}/")).header("authorization", auth).body(body);
    for (k, v) in &headers {
        if *k != "host" {
            req = req.header(*k, v);
        }
    }
    let resp = req.send().await.with_context(|| format!("reaching AWS {service} in {region}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = xml_tag(&text, "Message").or_else(|| serde_json::from_str::<Value>(&text).ok().and_then(|v| v["message"].as_str().or(v["Message"].as_str()).map(String::from)));
        bail!("AWS: {}", msg.unwrap_or_else(|| status.to_string()));
    }
    Ok(text)
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a str,
    headers: &'a [(&'a str, String)],
    body: &'a [u8],
}

/// AWS Signature Version 4: the Authorization header for a request.
fn sigv4(r: &Request, access_key: &str, secret_key: &str, region: &str, service: &str, amz_date: &str) -> Result<String> {
    let mut headers: Vec<(String, String)> = r.headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_string())).collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}", r.method, r.path, r.query, hex::encode(Sha256::digest(r.body)));
    let day = amz_date.get(..8).ok_or_else(|| anyhow!("bad date"))?;
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let mut key = format!("AWS4{secret_key}").into_bytes();
    for part in [day, region, service, "aws4_request"] {
        key = hmac(&key, part.as_bytes())?;
    }
    let signature = hex::encode(hmac(&key, to_sign.as_bytes())?);
    Ok(format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}"))
}

fn hmac(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).map_err(|e| anyhow!("{e}"))?;
    m.update(data);
    Ok(m.finalize().into_bytes().to_vec())
}

/// Percent-encoding as AWS wants it: everything but A-Z a-z 0-9 - _ . ~.
fn aws_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example in AWS's documentation ("Create a signed AWS API request").
    #[test]
    fn sigv4_matches_the_aws_example() {
        let headers = vec![
            ("content-type", "application/x-www-form-urlencoded; charset=utf-8".to_string()),
            ("host", "iam.amazonaws.com".to_string()),
            ("x-amz-date", "20150830T123600Z".to_string()),
        ];
        let r = Request { method: "GET", path: "/", query: "Action=ListUsers&Version=2010-05-08", headers: &headers, body: b"" };
        let auth = sigv4(&r, "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", "us-east-1", "iam", "20150830T123600Z").unwrap();
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn encoding_and_xml() {
        assert_eq!(aws_encode("a b+c/=~"), "a%20b%2Bc%2F%3D~");
        assert_eq!(xml_tag("<x><instanceId>i-123</instanceId></x>", "instanceId").as_deref(), Some("i-123"));
        assert!(aws_arm("t4g.small") && aws_arm("m7gd.large") && !aws_arm("t3.small") && !aws_arm("m7i.large"));
    }

    #[test]
    fn pools_grow_and_shrink() {
        let p = Pool {
            id: "p".into(),
            name: "p".into(),
            account_id: "a".into(),
            region: "fsn1".into(),
            size: "cx22".into(),
            labels: Value::Null,
            min_servers: 1,
            max_servers: 3,
            scale_up_cpu: 75.0,
            scale_down_cpu: 25.0,
        };
        assert_eq!(pool_step(&p, 0, false, None, 0, 0), PoolStep::Grow);
        assert_eq!(pool_step(&p, 1, true, Some(99.0), 0, 9999), PoolStep::Wait);
        assert_eq!(pool_step(&p, 1, false, Some(90.0), 0, 9999), PoolStep::Grow);
        assert_eq!(pool_step(&p, 1, false, Some(90.0), 0, 60), PoolStep::Wait);
        assert_eq!(pool_step(&p, 3, false, Some(90.0), 0, 9999), PoolStep::Wait);
        assert_eq!(pool_step(&p, 2, false, Some(10.0), 600, 9999), PoolStep::Wait);
        assert_eq!(pool_step(&p, 2, false, Some(10.0), SHRINK_AFTER_S, 9999), PoolStep::Shrink);
        assert_eq!(pool_step(&p, 1, false, Some(10.0), SHRINK_AFTER_S, 9999), PoolStep::Wait);
        assert_eq!(pool_step(&Pool { max_servers: 1, ..p.clone() }, 2, false, Some(50.0), 0, 0), PoolStep::Shrink);
    }
}
