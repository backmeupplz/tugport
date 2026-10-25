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
    /// List the servers and their status.
    Servers,
    /// List the services of a project.
    Services { project: String },
    /// Show a service's status and replicas.
    Status { service: String },
    /// Deploy a service and stream its log until it finishes.
    Deploy {
        service: String,
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Build without the layer cache and pull base images again.
        #[arg(long)]
        no_cache: bool,
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
    /// Run a command inside a service's container. A single argument is run as a shell command
    /// line (`tugport exec web 'ls | wc -l'`); several are passed as separate words.
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

fn config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".config"));
    // Without HOME the key would land in a folder relative to wherever the command runs.
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or(home).ok_or_else(|| anyhow!("set HOME (or XDG_CONFIG_HOME) so the key has a private place"))?;
    Ok(base.join("tugport").join("cli.json"))
}

/// Text from the panel (build output, container logs) as it may be printed on a terminal: no
/// control sequences, which could rewrite the screen or set the clipboard.
fn shown(text: &str) -> std::borrow::Cow<'_, str> {
    use std::io::IsTerminal;
    let bad = |c: char| c.is_control() && c != '\n' && c != '\t';
    if !std::io::stdout().is_terminal() || !text.chars().any(bad) {
        return text.into();
    }
    text.chars().map(|c| if bad(c) { '\u{fffd}' } else { c }).collect::<String>().into()
}

/// Several words for `sh -c`, each quoted, so none is read as shell syntax.
fn shell_words(words: &[String]) -> String {
    if let [line] = words {
        return line.clone();
    }
    let quote = |w: &String| {
        if !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./:=@%+,-".contains(&b)) { w.clone() } else { format!("'{}'", w.replace('\'', "'\\''")) }
    };
    words.iter().map(quote).collect::<Vec<_>>().join(" ")
}

fn config() -> Result<Config> {
    if let (Ok(url), Ok(token)) = (std::env::var("TUGPORT_URL"), std::env::var("TUGPORT_TOKEN")) {
        return Ok(Config { url, token });
    }
    let text = std::fs::read_to_string(config_path()?).context("not logged in; run `tugport login <url> --token <key>` or set TUGPORT_URL and TUGPORT_TOKEN")?;
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
            bail!("{}", shown(v["error"].as_str().unwrap_or(status.as_str())));
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
        // Bytes until a whole event is in: a character split across chunks must stay whole.
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
                let block: Vec<u8> = buf.drain(..end + 2).collect();
                let block = String::from_utf8_lossy(&block);
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

/// Print `v` as JSON, or for people with `human`; exit code 0.
fn print(json: bool, v: &Value, human: impl FnOnce(&Value)) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        human(v);
    }
    Ok(0)
}

/// A string field, or "" when it is missing.
fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn time(ts: &Value) -> String {
    chrono::DateTime::from_timestamp_millis(ts.as_i64().unwrap_or(0)).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default()
}

/// Run a client command; returns the process exit code.
pub async fn run(cmd: Client, json: bool) -> Result<i32> {
    let api = Api::new(match &cmd {
        Client::Login { url, token } => Config { url: url.clone(), token: token.clone() },
        _ => config()?,
    })?;
    match cmd {
        Client::Login { url, .. } => {
            let me = api.get("/api/me").await.context("the key was not accepted")?;
            let path = config_path()?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            crate::util::write_private(&path, serde_json::to_string_pretty(&api.cfg)?.as_bytes())?;
            println!("Logged in to {url} as {}.", me["principal"]["name"].as_str().unwrap_or("?"));
            Ok(0)
        }
        Client::Projects => print(json, &api.get("/api/projects").await?, |v| {
            for p in v["projects"].as_array().into_iter().flatten() {
                println!("{:<24} {:>3} services  {}", s(&p["name"]), p["services"], s(&p["role"]));
            }
        }),
        Client::Servers => print(json, &api.get("/api/nodes").await?, |v| {
            for n in v["nodes"].as_array().into_iter().flatten() {
                let i = &n["info"];
                println!(
                    "{:<24} {:<12} {:<15} {:>3} cpus {:>6.1} GB  {}",
                    s(&n["name"]),
                    s(&n["status"]),
                    n["mesh_ip"].as_str().unwrap_or("-"),
                    i["cpus"].as_u64().unwrap_or(0),
                    i["memory_total"].as_f64().unwrap_or(0.0) / 1e9,
                    s(&n["region"]),
                );
            }
        }),
        Client::Services { project } => print(json, &api.get(&format!("/api/projects/{project}")).await?, |v| {
            for svc in v["services"].as_array().into_iter().flatten() {
                println!("{:<24} {:<9} {}", s(&svc["name"]), s(&svc["kind"]), s(&svc["status"]["state"]));
            }
        }),
        Client::Status { service } => print(json, &api.get(&format!("/api/services/{service}")).await?, |v| {
            let st = &v["status"];
            println!("{} is {} ({}/{} running)", service, st["state"].as_str().unwrap_or("?"), st["running"], st["desired"]);
            for d in v["domains"].as_array().into_iter().flatten() {
                println!("  https://{}{}", s(&d["host"]), s(&d["path"]));
            }
            for r in st["replicas"].as_array().into_iter().flatten() {
                println!("  {} {} {}", s(&r["name"]), s(&r["state"]), s(&r["health"]));
            }
            if let Some(d) = v["last_deploy"].as_object() {
                println!("  last deploy {} ({})", d.get("status").map_or("", s), d.get("trigger").map_or("", s));
            }
        }),
        Client::Deploy { service, git_ref, no_cache, detach } => {
            let v = api.post(&format!("/api/services/{service}/deploy"), json!({ "ref": git_ref, "no_cache": no_cache })).await?;
            let id = v["deploy_id"].as_str().ok_or_else(|| anyhow!("no deploy id returned"))?.to_string();
            if detach {
                return print(json, &v, |_| println!("Deploy {id} queued."));
            }
            let mut status = String::from("queued");
            api.events(&format!("/api/deploys/{id}/log"), |event, data| {
                match event {
                    "line" => {
                        if let Ok(l) = serde_json::from_str::<Value>(data) {
                            if json {
                                println!("{data}");
                            } else {
                                println!("{} {}", time(&l["ts"]), shown(s(&l["line"])));
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
                path.push_str(&format!("&q={}", crate::util::urlencode(q)));
            }
            let v = api.get(&path).await?;
            let show = |l: &Value| {
                if json {
                    println!("{l}");
                } else {
                    println!("{} {} {}", time(&l["ts"]), s(&l["replica"]), shown(s(&l["line"])));
                }
            };
            for l in v["lines"].as_array().into_iter().flatten() {
                show(l);
            }
            if follow {
                let svc = api.get(&format!("/api/services/{service}")).await?;
                let id = svc["service"]["id"].as_str().unwrap_or(&service).to_string();
                let q = q.map(|q| q.to_lowercase());
                api.events(&format!("/api/services/{id}/logs/stream"), |_, data| {
                    if let Ok(l) = serde_json::from_str::<Value>(data)
                        && q.as_ref().is_none_or(|q| s(&l["line"]).to_lowercase().contains(q))
                    {
                        show(&l);
                    }
                    true
                })
                .await?;
            }
            Ok(0)
        }
        Client::Env { service, action, pairs } => match action.as_deref() {
            None => print(json, &api.get(&format!("/api/services/{service}/env")).await?, |v| match &v["env"] {
                Value::Object(m) => m.iter().for_each(|(k, val)| println!("{k}={}", s(val))),
                Value::Array(a) => a.iter().for_each(|k| println!("{}", s(k))),
                _ => {}
            }),
            Some(verb @ ("set" | "unset")) => {
                let body = pairs
                    .iter()
                    .map(|p| match verb {
                        "set" => p.split_once('=').map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).ok_or_else(|| anyhow!("use NAME=value, got {p}")),
                        _ => Ok((p.clone(), Value::Null)),
                    })
                    .collect::<Result<serde_json::Map<_, _>>>()?;
                api.send(reqwest::Method::PUT, &format!("/api/services/{service}/env"), Some(Value::Object(body))).await?;
                println!("{}. Redeploy {service} to apply.", if verb == "set" { "Saved" } else { "Removed" });
                Ok(0)
            }
            Some(other) => bail!("unknown env action {other}; use set or unset"),
        },
        Client::Rollback { service } => print(json, &api.post(&format!("/api/services/{service}/rollback"), json!({})).await?, |v| {
            println!("Rolling back to {} (deploy {}).", s(&v["image"]), s(&v["deploy_id"]))
        }),
        Client::Restart { service } => action(&api, json, &service, "restart").await,
        Client::Stop { service } => action(&api, json, &service, "stop").await,
        Client::Start { service } => action(&api, json, &service, "start").await,
        Client::Exec { service, command } => {
            let v = api.post(&format!("/api/services/{service}/exec"), json!({ "command": shell_words(&command) })).await?;
            print(json, &v, |v| print!("{}", shown(s(&v["output"]))))?;
            Ok(v["exit_code"].as_i64().and_then(|c| i32::try_from(c).ok()).unwrap_or(1))
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
                println!("Created agent {name}. Its key is shown only once:\n\n  {}\n", s(&v["key"]["token"]));
                println!("{}", s(&v["snippets"]["env"]));
            })
        }
    }
}

async fn action(api: &Api, json: bool, service: &str, verb: &str) -> Result<i32> {
    let v = api.post(&format!("/api/services/{service}/{verb}"), json!({})).await?;
    print(json, &v, |_| println!("{service}: {verb} done."))
}

#[cfg(test)]
mod tests {
    use super::shell_words;

    #[test]
    fn exec_words_stay_words() {
        let w = |a: &[&str]| shell_words(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(w(&["ls | wc -l"]), "ls | wc -l");
        assert_eq!(w(&["grep", "a; curl x|sh", "f"]), "grep 'a; curl x|sh' f");
        assert_eq!(w(&["echo", "it's"]), "echo 'it'\\''s'");
    }
}
