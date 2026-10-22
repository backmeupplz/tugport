//! Command-line client for a Tugport panel (same binary): `tugport login`, `deploy`, `logs`...

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Client {
    /// Save the panel address and an API key for the other commands.
    Login {
        url: String,
        #[arg(long)]
        token: String,
    },
    /// List projects.
    Projects,
    /// List the services of a project.
    Services { project: String },
    /// Show a service's status and replicas.
    Status { service: String },
    /// Deploy a service and stream its log until it finishes.
    Deploy {
        service: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Return right away instead of following the deploy.
        #[arg(long)]
        detach: bool,
    },
    /// Show a service's recent logs, or follow them with -f.
    Logs {
        service: String,
        #[arg(short, long)]
        follow: bool,
        /// Only lines containing this text.
        #[arg(short)]
        q: Option<String>,
        #[arg(short = 'n', default_value_t = 100)]
        lines: usize,
    },
    /// List env var names, or `set NAME=value...`, or `unset NAME...`.
    Env { service: String, action: Option<String>, pairs: Vec<String> },
    /// Roll back to the previous version.
    Rollback { service: String },
    Restart { service: String },
    Stop { service: String },
    Start { service: String },
    /// Run a command inside a service's container.
    Exec {
        service: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Create an agent user and print its API key.
    Agent {
        name: String,
        /// Role on every project, including future ones: viewer, deployer, operator, admin.
        #[arg(long)]
        all: Option<String>,
        /// PROJECT:ROLE, repeatable.
        #[arg(long = "project")]
        projects: Vec<String>,
        /// secrets:read or destroy, repeatable.
        #[arg(long)]
        grant: Vec<String>,
        #[arg(long)]
        expires_days: Option<u32>,
    },
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    url: String,
    token: String,
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("tugport").join("cli.json")
}

fn config() -> Result<Config> {
    if let (Ok(url), Ok(token)) = (std::env::var("TUGPORT_URL"), std::env::var("TUGPORT_TOKEN")) {
        return Ok(Config { url, token });
    }
    let text = std::fs::read_to_string(config_path()).context("not logged in; run `tugport login <url> --token <key>` or set TUGPORT_URL and TUGPORT_TOKEN")?;
    Ok(serde_json::from_str(&text)?)
}

struct Api {
    cfg: Config,
    http: reqwest::Client,
}

impl Api {
    fn new(cfg: Config) -> Result<Self> {
        Ok(Api { cfg, http: reqwest::Client::builder().build()? })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.url.trim_end_matches('/'))
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method, self.url(path)).bearer_auth(&self.cfg.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.with_context(|| format!("connecting to {}", self.cfg.url))?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}", v["error"].as_str().unwrap_or(status.as_str()));
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    /// Server-sent events as (event, data) pairs; stops when `f` returns false.
    async fn events(&self, path: &str, mut f: impl FnMut(&str, &str) -> bool) -> Result<()> {
        let resp = self.http.get(self.url(path)).bearer_auth(&self.cfg.token).send().await?.error_for_status()?;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                let mut event = "message";
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(e) = line.strip_prefix("event:") {
                        event = e.trim();
                    } else if let Some(d) = line.strip_prefix("data:") {
                        data.push_str(d.trim_start());
                    }
                }
                if !data.is_empty() && !f(event, &data) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

fn print(json: bool, v: &Value, human: impl FnOnce(&Value)) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        human(v);
    }
}

fn time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default()
}

/// Run a client command; returns the process exit code.
pub async fn run(cmd: Client, json: bool) -> Result<i32> {
    if let Client::Login { url, token } = &cmd {
        let api = Api::new(Config { url: url.clone(), token: token.clone() })?;
        let me = api.get("/api/me").await.context("the key was not accepted")?;
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
            f.write_all(serde_json::to_string_pretty(&Config { url: url.clone(), token: token.clone() })?.as_bytes())?;
        }
        println!("Logged in to {url} as {}.", me["principal"]["name"].as_str().unwrap_or("?"));
        return Ok(0);
    }
    let api = Api::new(config()?)?;
    match cmd {
        Client::Login { .. } => Ok(0),
        Client::Projects => {
            let v = api.get("/api/projects").await?;
            print(json, &v, |v| {
                for p in v["projects"].as_array().into_iter().flatten() {
                    println!("{:<24} {:>3} services  {}", p["name"].as_str().unwrap_or(""), p["services"], p["role"].as_str().unwrap_or(""));
                }
            });
            Ok(0)
        }
        Client::Services { project } => {
            let v = api.get(&format!("/api/projects/{project}")).await?;
            print(json, &v, |v| {
                for s in v["services"].as_array().into_iter().flatten() {
                    println!(
                        "{:<24} {:<9} {}",
                        s["name"].as_str().unwrap_or(""),
                        s["kind"].as_str().unwrap_or(""),
                        s["status"]["state"].as_str().unwrap_or("")
                    );
                }
            });
            Ok(0)
        }
        Client::Status { service } => {
            let v = api.get(&format!("/api/services/{service}")).await?;
            print(json, &v, |v| {
                let st = &v["status"];
                println!("{} is {} ({}/{} running)", service, st["state"].as_str().unwrap_or("?"), st["running"], st["desired"]);
                for d in v["domains"].as_array().into_iter().flatten() {
                    println!("  https://{}{}", d["host"].as_str().unwrap_or(""), d["path"].as_str().unwrap_or(""));
                }
                for r in st["replicas"].as_array().into_iter().flatten() {
                    println!("  {} {} {}", r["name"].as_str().unwrap_or(""), r["state"].as_str().unwrap_or(""), r["health"].as_str().unwrap_or(""));
                }
                if let Some(d) = v["last_deploy"].as_object() {
                    println!("  last deploy {} ({})", d.get("status").and_then(|s| s.as_str()).unwrap_or(""), d.get("trigger").and_then(|s| s.as_str()).unwrap_or(""));
                }
            });
            Ok(0)
        }
        Client::Deploy { service, git_ref, detach } => {
            let v = api.post(&format!("/api/services/{service}/deploy"), json!({ "ref": git_ref })).await?;
            let id = v["deploy_id"].as_str().ok_or_else(|| anyhow!("no deploy id returned"))?.to_string();
            if detach {
                print(json, &v, |_| println!("Deploy {id} queued."));
                return Ok(0);
            }
            let mut status = String::from("queued");
            api.events(&format!("/api/deploys/{id}/log"), |event, data| {
                match event {
                    "line" => {
                        if let Ok(l) = serde_json::from_str::<Value>(data) {
                            if json {
                                println!("{data}");
                            } else {
                                println!("{} {}", time(l["ts"].as_i64().unwrap_or(0)), l["line"].as_str().unwrap_or(""));
                            }
                        }
                        true
                    }
                    "status" => {
                        status = data.to_string();
                        !crate::deploy::FINAL.contains(&data)
                    }
                    _ => true,
                }
            })
            .await?;
            if !json {
                println!("Deploy {id}: {status}");
            }
            Ok(if status == "live" { 0 } else { 1 })
        }
        Client::Logs { service, follow, q, lines } => {
            let mut path = format!("/api/services/{service}/logs?limit={lines}");
            if let Some(q) = &q {
                path.push_str(&format!("&q={}", urlencode(q)));
            }
            let v = api.get(&path).await?;
            let show = |l: &Value| {
                if json {
                    println!("{l}");
                } else {
                    println!("{} {} {}", time(l["ts"].as_i64().unwrap_or(0)), l["replica"].as_str().unwrap_or(""), l["line"].as_str().unwrap_or(""));
                }
            };
            for l in v["lines"].as_array().into_iter().flatten() {
                show(l);
            }
            if follow {
                let svc = api.get(&format!("/api/services/{service}")).await?;
                let id = svc["service"]["id"].as_str().unwrap_or(&service).to_string();
                api.events(&format!("/api/services/{id}/logs/stream"), |_, data| {
                    if let Ok(l) = serde_json::from_str::<Value>(data)
                        && q.as_ref().is_none_or(|q| l["line"].as_str().unwrap_or("").to_lowercase().contains(&q.to_lowercase()))
                    {
                        show(&l);
                    }
                    true
                })
                .await?;
            }
            Ok(0)
        }
        Client::Env { service, action, pairs } => {
            match action.as_deref() {
                None => {
                    let v = api.get(&format!("/api/services/{service}/env")).await?;
                    print(json, &v, |v| match &v["env"] {
                        Value::Object(m) => m.iter().for_each(|(k, val)| println!("{k}={}", val.as_str().unwrap_or(""))),
                        Value::Array(a) => a.iter().for_each(|k| println!("{}", k.as_str().unwrap_or(""))),
                        _ => {}
                    });
                }
                Some("set") => {
                    let mut body = serde_json::Map::new();
                    for p in &pairs {
                        let (k, v) = p.split_once('=').ok_or_else(|| anyhow!("use NAME=value, got {p}"))?;
                        body.insert(k.to_string(), Value::String(v.to_string()));
                    }
                    api.send(reqwest::Method::PUT, &format!("/api/services/{service}/env"), Some(Value::Object(body))).await?;
                    println!("Saved. Redeploy {service} to apply.");
                }
                Some("unset") => {
                    let body: serde_json::Map<String, Value> = pairs.iter().map(|k| (k.clone(), Value::Null)).collect();
                    api.send(reqwest::Method::PUT, &format!("/api/services/{service}/env"), Some(Value::Object(body))).await?;
                    println!("Removed. Redeploy {service} to apply.");
                }
                Some(other) => bail!("unknown env action {other}; use set or unset"),
            }
            Ok(0)
        }
        Client::Rollback { service } => {
            let v = api.post(&format!("/api/services/{service}/rollback"), json!({})).await?;
            print(json, &v, |v| println!("Rolling back to {} (deploy {}).", v["image"].as_str().unwrap_or(""), v["deploy_id"].as_str().unwrap_or("")));
            Ok(0)
        }
        Client::Restart { service } => action(&api, json, &service, "restart").await,
        Client::Stop { service } => action(&api, json, &service, "stop").await,
        Client::Start { service } => action(&api, json, &service, "start").await,
        Client::Exec { service, command } => {
            let v = api.post(&format!("/api/services/{service}/exec"), json!({ "command": command.join(" ") })).await?;
            print(json, &v, |v| print!("{}", v["output"].as_str().unwrap_or("")));
            Ok(v["exit_code"].as_i64().unwrap_or(1) as i32)
        }
        Client::Agent { name, all, projects, grant, expires_days } => {
            let mut map = serde_json::Map::new();
            for p in &projects {
                let (proj, role) = p.split_once(':').ok_or_else(|| anyhow!("use PROJECT:ROLE, got {p}"))?;
                map.insert(proj.to_string(), Value::String(role.to_string()));
            }
            let v = api
                .post("/api/agents", json!({ "name": name, "all_projects_role": all, "projects": map, "grants": grant, "key": { "expires_days": expires_days } }))
                .await?;
            print(json, &v, |v| {
                println!("Created agent {name}. Its key is shown only once:\n\n  {}\n", v["key"]["token"].as_str().unwrap_or(""));
                println!("{}", v["snippets"]["env"].as_str().unwrap_or(""));
            });
            Ok(0)
        }
    }
}

async fn action(api: &Api, json: bool, service: &str, verb: &str) -> Result<i32> {
    let v = api.post(&format!("/api/services/{service}/{verb}"), json!({})).await?;
    print(json, &v, |_| println!("{service}: {verb} done."));
    Ok(0)
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
