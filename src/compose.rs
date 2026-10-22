//! Docker Compose stacks, run with the `docker compose` plugin that ships with Docker.
//!
//! Tugport writes its own copy of the compose file with labels, the project network
//! and log limits added, and passes the service's variables to compose for `${VAR}`
//! interpolation, exactly like a plain `docker compose up`.
// ponytail: variables reach compose containers through interpolation, so (unlike app
// services) they are visible in `docker inspect`; wrap each container with the init
// shim when that matters.

use crate::app::{App, AppState};
use crate::deploy::{Deploy, DeployLog};
use crate::model::Source;
use crate::runtime;
use crate::store::{self, Service};
use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::{Mapping, Value};
use tokio::process::Command;

pub fn project_name(s: &Service) -> String {
    s.slug()
}

pub async fn deploy(app: &AppState, s: &Service, d: &Deploy, log: &DeployLog) -> Result<()> {
    let work = app.paths.sources().join(&s.id);
    std::fs::create_dir_all(&work)?;
    let (dir, file) = match &s.spec.source {
        Source::Compose { yaml } => {
            let dir = work.join("inline");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("docker-compose.yml"), yaml)?;
            (dir.clone(), dir.join("docker-compose.yml"))
        }
        Source::Git { provider, repo, branch, path, .. } => {
            let git_ref = d.commit_sha.clone().or(d.git_ref.clone()).unwrap_or_else(|| branch.clone());
            let checkout = work.join("repo");
            let (sha, msg) = crate::git::checkout(app, s, provider.as_deref(), repo, &git_ref, &checkout, &|l| log.line(l)).await?;
            app.db.conn().execute("UPDATE deploys SET commit_sha = ?1, commit_message = ?2 WHERE id = ?3", rusqlite::params![sha, msg, d.id])?;
            let dir = checkout.join(path.trim_matches('/'));
            let name = s.spec.compose.as_ref().map(|c| c.file.clone()).unwrap_or_else(|| "docker-compose.yml".into());
            (dir.clone(), dir.join(name))
        }
        _ => bail!("Compose stacks come from pasted YAML or a Git repository"),
    };
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let network = runtime::ensure_network(app, &s.project).await?;
    let rewritten = rewrite(&text, s, &d.id, &network)?;
    let generated = dir.join(".tugport-compose.yml");
    std::fs::write(&generated, rewritten)?;

    let env = store::resolved_env(app, s)?;
    let compose = |args: &[&str]| {
        let mut c = Command::new("docker");
        c.args(["compose", "-p", &project_name(s), "-f"]).arg(&generated).args(args).current_dir(&dir);
        c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        c
    };
    log.line("Pulling images");
    crate::proc::run_streamed(compose(&["pull", "--ignore-buildable", "--quiet"]), &|l| log.line(l)).await?;
    log.line("Starting the stack");
    crate::proc::run_streamed(compose(&["up", "-d", "--build", "--remove-orphans", "--wait", "--wait-timeout", "300"]), &|l| log.line(l))
        .await
        .context("the stack did not become healthy")?;
    runtime::set_live_deploy(app, &s.id, &d.id)?;
    crate::proxy::reload(app);
    Ok(())
}

/// Add Tugport labels, the project network and log limits to every compose service.
pub fn rewrite(text: &str, s: &Service, deploy_id: &str, network: &str) -> Result<String> {
    let mut doc: Value = serde_yaml::from_str(text).context("the compose file is not valid YAML")?;
    let services = doc.get_mut("services").and_then(|v| v.as_mapping_mut()).ok_or_else(|| anyhow!("the compose file has no services"))?;
    for (name, svc) in services.iter_mut() {
        let name = name.as_str().unwrap_or_default().to_string();
        let Some(svc) = svc.as_mapping_mut() else { continue };
        let Some(labels) = entry_map(svc, "labels") else { continue };
        for (k, v) in [
            (runtime::L_MANAGED, "true".to_string()),
            (runtime::L_PROJECT, s.project_id.clone()),
            (runtime::L_SERVICE, s.id.clone()),
            (runtime::L_DEPLOY, deploy_id.to_string()),
            (runtime::L_NODE, "local".to_string()),
        ] {
            labels.insert(Value::String(k.into()), Value::String(v));
        }
        if !svc.contains_key("logging") {
            let mut options = Mapping::new();
            options.insert("max-size".into(), "10m".into());
            options.insert("max-file".into(), "3".into());
            let mut logging = Mapping::new();
            logging.insert("driver".into(), "json-file".into());
            logging.insert("options".into(), Value::Mapping(options));
            svc.insert("logging".into(), Value::Mapping(logging));
        }
        if !svc.contains_key("restart") {
            svc.insert("restart".into(), "unless-stopped".into());
        }
        if svc.contains_key("network_mode") {
            continue;
        }
        let alias = format!("{}-{name}", s.name);
        let nets = svc.entry("networks".into()).or_insert_with(|| Value::Sequence(vec!["default".into()]));
        if let Value::Sequence(list) = nets {
            let mut m = Mapping::new();
            for n in list.drain(..) {
                m.insert(n, Value::Null);
            }
            *nets = Value::Mapping(m);
        }
        if let Value::Mapping(m) = nets {
            let mut cfg = Mapping::new();
            cfg.insert("aliases".into(), Value::Sequence(vec![Value::String(alias)]));
            m.insert(Value::String(network.into()), Value::Mapping(cfg));
        }
    }
    let root = doc.as_mapping_mut().ok_or_else(|| anyhow!("the compose file is not a mapping"))?;
    let networks = root.entry("networks".into()).or_insert_with(|| Value::Mapping(Mapping::new()));
    if networks.is_null() {
        *networks = Value::Mapping(Mapping::new());
    }
    if let Value::Mapping(m) = networks {
        let mut ext = Mapping::new();
        ext.insert("external".into(), Value::Bool(true));
        m.insert(Value::String(network.into()), Value::Mapping(ext));
    }
    Ok(serde_yaml::to_string(&doc)?)
}

/// The mapping under `key`, converting the `["a=b"]` list form; None if it holds something else.
fn entry_map<'a>(svc: &'a mut Mapping, key: &str) -> Option<&'a mut Mapping> {
    let v = svc.entry(key.into()).or_insert_with(|| Value::Mapping(Mapping::new()));
    match v {
        Value::Null => *v = Value::Mapping(Mapping::new()),
        Value::Sequence(list) => {
            let mut m = Mapping::new();
            for item in list.drain(..) {
                if let Some((k, val)) = item.as_str().and_then(|s| s.split_once('=')) {
                    m.insert(k.into(), val.into());
                }
            }
            *v = Value::Mapping(m);
        }
        _ => {}
    }
    v.as_mapping_mut()
}

pub async fn down(app: &App, s: &Service, volumes: bool) -> Result<()> {
    let mut c = Command::new("docker");
    c.args(["compose", "-p", &project_name(s), "down", "--remove-orphans"]);
    if volumes {
        c.arg("--volumes");
    }
    let _ = app;
    crate::proc::output(c).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceSpec;

    #[test]
    fn rewrites_compose() {
        let s = Service {
            id: "svc1".into(),
            project_id: "p1".into(),
            project: "shop".into(),
            name: "plausible".into(),
            kind: "compose".into(),
            spec: ServiceSpec::default(),
            created_at: 0,
            updated_at: 0,
        };
        let out = rewrite("services:\n  web:\n    image: nginx\n    labels: [\"x=y\"]\n  db:\n    image: postgres\n    networks: [internal]\nnetworks:\n  internal: {}\n", &s, "d1", "tp-shop").unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["services"]["web"]["labels"]["x"].as_str(), Some("y"));
        assert_eq!(v["services"]["web"]["labels"]["tugport.service"].as_str(), Some("svc1"));
        assert_eq!(v["services"]["web"]["networks"]["tp-shop"]["aliases"][0].as_str(), Some("plausible-web"));
        assert!(v["services"]["db"]["networks"].as_mapping().unwrap().contains_key("internal"));
        assert_eq!(v["networks"]["tp-shop"]["external"].as_bool(), Some(true));
    }
}
