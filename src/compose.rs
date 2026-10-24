//! Docker Compose stacks, run with the `docker compose` plugin that ships with Docker.
//!
//! Tugport writes its own copy of the compose file with labels, the project network
//! and log limits added, and passes the service's variables to compose for `${VAR}`
//! interpolation, exactly like a plain `docker compose up`.
// ponytail: variables reach compose containers through interpolation, so (unlike app
// services) they are visible in `docker inspect`; wrap each container with the init
// shim when that matters.

use crate::app::AppState;
use crate::deploy::{Deploy, DeployLog};
use crate::model::Source;
use crate::runtime;
use crate::store::{self, Service};
use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::{Mapping, Value};
use tokio::process::Command;

pub async fn deploy(app: &AppState, s: &Service, d: &Deploy, log: &DeployLog) -> Result<()> {
    let (dir, file) = match &s.spec.source {
        Source::Compose { yaml } => crate::builder::write_inline(app, s, "docker-compose.yml", yaml)?,
        Source::Git { .. } => {
            let (dir, _) = crate::git::checkout(app, s, d, &|l| log.line(l)).await?;
            let name = s.spec.compose.as_ref().map_or("docker-compose.yml", |c| c.file.as_str());
            let file = crate::util::inside(&dir, name).ok_or_else(|| anyhow!("the compose file {name} must be inside the repository"))?;
            (dir, file)
        }
        _ => bail!("Compose stacks come from pasted YAML or a Git repository"),
    };
    let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let network = runtime::ensure_network(app, &s.project).await?;
    let rewritten = rewrite(&text, s, &d.id, &network)?;
    let generated = dir.join(".tugport-compose.yml");
    std::fs::write(&generated, rewritten)?;

    let env = store::resolved_env(app, s)?;
    // Interpolation reads the stack's own .env only, never files named by COMPOSE_ENV_FILES.
    let dotenv = dir.join(".env");
    let env_file = if dotenv.is_file() { dotenv } else { std::path::PathBuf::from("/dev/null") };
    let compose = |args: &[&str]| {
        let mut c = Command::new("docker");
        c.args(["compose", "-p", &s.slug(), "-f"]).arg(&generated).arg("--env-file").arg(&env_file).args(args).current_dir(&dir);
        // The variables feed `${VAR}` interpolation; ones like PATH or DOCKER_CONFIG would steer docker itself.
        c.envs(crate::proc::tool_env(&env));
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
fn rewrite(text: &str, s: &Service, deploy_id: &str, network: &str) -> Result<String> {
    let mut doc = parse(text)?;
    if !s.spec.privileged {
        check_untrusted(&doc)?;
    }
    let services = doc.get_mut("services").and_then(|v| v.as_mapping_mut()).ok_or_else(|| anyhow!("the compose file has no services"))?;
    for (name, svc) in services.iter_mut() {
        let Some(svc) = svc.as_mapping_mut() else { continue };
        let Some(labels) = entry_map(svc, "labels") else { continue };
        // Only Tugport says which service a container belongs to (routing and logs follow it).
        labels.retain(|k, _| !k.as_str().is_some_and(|k| k.starts_with("tugport.")));
        for (k, v) in runtime::labels(s, deploy_id, "local") {
            labels.insert(k.into(), v.into());
        }
        if !svc.contains_key("logging") {
            let options = runtime::LOG_LIMITS.iter().map(|&(k, v)| (k.into(), v.into())).collect();
            svc.insert("logging".into(), Value::Mapping(Mapping::from_iter([("driver".into(), "json-file".into()), ("options".into(), Value::Mapping(options))])));
        }
        if !svc.contains_key("restart") {
            svc.insert("restart".into(), "unless-stopped".into());
        }
        if svc.contains_key("network_mode") {
            continue;
        }
        let alias = format!("{}-{}", s.name, name.as_str().unwrap_or_default());
        let nets = svc.entry("networks".into()).or_insert_with(|| Value::Sequence(vec!["default".into()]));
        if let Value::Sequence(list) = nets {
            *nets = Value::Mapping(list.drain(..).map(|n| (n, Value::Null)).collect());
        }
        if let Value::Mapping(m) = nets {
            m.insert(network.into(), Value::Mapping(Mapping::from_iter([("aliases".into(), Value::Sequence(vec![alias.into()]))])));
        }
    }
    let root = doc.as_mapping_mut().ok_or_else(|| anyhow!("the compose file is not a mapping"))?;
    let networks = root.entry("networks".into()).or_insert_with(|| Value::Mapping(Mapping::new()));
    if networks.is_null() {
        *networks = Value::Mapping(Mapping::new());
    }
    if let Value::Mapping(m) = networks {
        m.insert(network.into(), Value::Mapping(Mapping::from_iter([("external".into(), Value::Bool(true))])));
    }
    Ok(serde_yaml::to_string(&doc)?)
}

/// Without the service's privileged switch (which only admins with the destroy grant can turn
/// on), a stack may use only these keys. The others reach the host or other projects:
/// privileged mode, capabilities, devices, host namespaces, the Docker socket, providers that
/// run host programs... An allowlist, so keys that Compose adds later are refused, not missed.
const SERVICE_KEYS: &[&str] = &[
    "annotations", "attach", "blkio_config", "build", "cap_drop", "cgroup", "command", "configs", "container_name", "cpu_count",
    "cpu_percent", "cpu_period", "cpu_quota", "cpu_shares", "cpus", "cpuset", "depends_on", "deploy", "develop", "dns", "dns_opt",
    "dns_search", "domainname", "entrypoint", "env_file", "environment", "expose", "extends", "extra_hosts", "gpus", "group_add",
    "healthcheck", "hostname", "image", "init", "ipc", "label_file", "labels", "links", "logging", "mac_address", "mem_limit",
    "mem_reservation", "mem_swappiness", "memswap_limit", "network_mode", "networks", "pid", "pids_limit", "platform", "ports",
    "post_start", "pre_stop", "profiles", "pull_policy", "read_only", "restart", "scale", "secrets", "security_opt", "shm_size",
    "stdin_open", "stop_grace_period", "stop_signal", "storage_opt", "sysctls", "tmpfs", "tty", "ulimits", "user", "volumes",
    "volumes_from", "working_dir",
];
const BUILD_KEYS: &[&str] = &[
    "additional_contexts", "args", "cache_from", "context", "dockerfile", "dockerfile_inline", "extra_hosts", "labels", "network",
    "no_cache", "platforms", "pull", "secrets", "shm_size", "tags", "target", "ulimits",
];

/// Whether a Compose file can run without privileged mode (and why not); for migrations.
pub fn check_untrusted_text(text: &str) -> Result<()> {
    check_untrusted(&parse(text)?)
}

/// Compose applies `<<` merges itself, so check (and hand it) the merged document.
fn parse(text: &str) -> Result<Value> {
    let mut doc: Value = serde_yaml::from_str(text).context("the compose file is not valid YAML")?;
    doc.apply_merge().context("the compose file has an invalid `<<` merge")?;
    Ok(doc)
}

fn privileged(what: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{what} needs the stack's privileged mode, which an admin with the destroy grant can turn on")
}

/// Refuse what an unprivileged stack must not do. Values that decide where a mount or a file
/// comes from may not use `${VAR}`, since interpolation would change them after this check.
fn check_untrusted(doc: &Value) -> Result<()> {
    let root = doc.as_mapping().ok_or_else(|| anyhow!("the compose file is not a mapping"))?;
    for (k, v) in root {
        let v = plain(v);
        match key(k)? {
            "version" | "name" => {}
            "services" => {
                for (name, svc) in v.as_mapping().into_iter().flatten() {
                    check_service(plain(svc)).map_err(|e| anyhow!("service {}: {e}", key(name).unwrap_or("?")))?;
                }
            }
            "networks" => each(v, |name, n| {
                only(n, &["driver", "labels", "internal", "attachable", "enable_ipv4", "enable_ipv6"], &format!("network {name}"))?;
                match str_at(n, "driver") {
                    None | Some("bridge") => Ok(()),
                    Some(_) => Err(privileged(format_args!("network {name}'s driver"))),
                }
            })?,
            "volumes" => each(v, |name, vol| only(vol, &["labels"], &format!("volume {name}")))?,
            "secrets" | "configs" => each(v, |name, sc| {
                only(sc, &["file", "environment", "content", "labels"], name)?;
                str_at(sc, "file").map_or(Ok(()), rel)
            })?,
            k if k.starts_with("x-") => {}
            k => return Err(privileged(k)),
        }
    }
    Ok(())
}

fn check_service(svc: &Value) -> Result<()> {
    let Some(m) = svc.as_mapping() else { return Ok(()) };
    for (k, v) in m {
        let (k, v) = (key(k)?, plain(v));
        if k.starts_with("x-") || (k == "privileged" && *v == Value::Bool(false)) {
            continue;
        }
        if !SERVICE_KEYS.contains(&k) {
            return Err(privileged(k));
        }
        match k {
            "build" => check_build(v)?,
            "env_file" | "label_file" => {
                for f in list(v) {
                    rel(plain(f).as_str().or_else(|| str_at(plain(f), "path")).unwrap_or("$"))?;
                }
            }
            "extends" if str_at(v, "file").is_some() => return Err(privileged("extends from another file")),
            "volumes" => list(v).into_iter().try_for_each(check_mount)?,
            "volumes_from" => {
                for f in list(v) {
                    if plain(f).as_str().is_none_or(|f| f.contains('$') || f.starts_with("container:")) {
                        return Err(privileged("volumes_from a container outside the stack"));
                    }
                }
            }
            "network_mode" => allowed(v, &["none"], "network_mode")?,
            "pid" => allowed(v, &[], "pid")?,
            "ipc" => allowed(v, &["private", "shareable", "none"], "ipc")?,
            "cgroup" => allowed(v, &["private"], "cgroup")?,
            "security_opt" => {
                for o in list(v) {
                    if plain(o).as_str().is_none_or(|o| !o.starts_with("no-new-privileges") || o.contains('$')) {
                        return Err(privileged("security_opt"));
                    }
                }
            }
            "post_start" | "pre_stop" => {
                for hook in list(v) {
                    if plain(hook).get("privileged").is_some_and(|p| *plain(p) != Value::Bool(false)) {
                        return Err(privileged("a privileged hook"));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_build(v: &Value) -> Result<()> {
    if let Some(c) = v.as_str() {
        return context(c);
    }
    for (k, val) in v.as_mapping().into_iter().flatten() {
        let (k, val) = (key(k)?, plain(val));
        if k.starts_with("x-") {
            continue;
        }
        if !BUILD_KEYS.contains(&k) {
            return Err(privileged(format_args!("build.{k}")));
        }
        match k {
            "context" => context(val.as_str().unwrap_or("$"))?,
            "dockerfile" => rel(val.as_str().unwrap_or("$"))?,
            "additional_contexts" => {
                let sources: Vec<&str> = match val {
                    Value::Mapping(m) => m.values().map(|c| plain(c).as_str().unwrap_or("$")).collect(),
                    _ => list(val).into_iter().map(|c| plain(c).as_str().and_then(|c| c.split_once('=')).map_or("$", |(_, c)| c)).collect(),
                };
                for c in sources {
                    if !(c.starts_with("docker-image://") || c.starts_with("service:")) {
                        context(c)?;
                    }
                }
            }
            "cache_from" => {
                for c in list(val) {
                    if plain(c).as_str().is_none_or(|c| c.contains("type=local") || c.contains('$')) {
                        bail!("build.cache_from may not read caches from this server's disk");
                    }
                }
            }
            "network" => allowed(val, &["default", "none"], "build.network")?,
            _ => {}
        }
    }
    Ok(())
}

/// A volume entry: named volumes are fine, host paths only inside the stack's folder.
fn check_mount(v: &Value) -> Result<()> {
    let v = plain(v);
    if let Some(entry) = v.as_str() {
        if entry.contains('$') {
            bail!("{entry}: volumes may not use variables");
        }
        return match entry.split_once(':') {
            Some((source, _)) if source.starts_with(['.', '/', '~']) => rel(source),
            _ => Ok(()),
        };
    }
    let source = str_at(v, "source").unwrap_or_default();
    if source.contains('$') {
        bail!("{source}: volumes may not use variables");
    }
    match str_at(v, "type").unwrap_or("volume") {
        "bind" => rel(source),
        "volume" | "tmpfs" | "image" => Ok(()),
        t => Err(privileged(format_args!("a {t} mount"))),
    }
}

/// A build context: a remote repository or URL, or a folder inside the stack's.
fn context(c: &str) -> Result<()> {
    if !c.contains('$') && ["https://", "http://", "git://", "git@", "ssh://"].iter().any(|p| c.starts_with(p)) { Ok(()) } else { rel(c) }
}

/// A path relative to the stack's folder that stays inside it.
fn rel(p: &str) -> Result<()> {
    let escapes = p.contains('$') || p.starts_with(['/', '~']) || std::path::Path::new(p).components().any(|c| c == std::path::Component::ParentDir);
    if escapes {
        bail!("{p}: only paths inside the stack's folder are allowed without privileged mode");
    }
    Ok(())
}

/// `v` is a string from `exact`, or starts with `service:` (shares another service of the stack).
fn allowed(v: &Value, exact: &[&str], what: &str) -> Result<()> {
    match v.as_str() {
        Some(x) if !x.contains('$') && (exact.contains(&x) || x.starts_with("service:")) => Ok(()),
        _ => Err(privileged(format_args!("{what}: {}", serde_yaml::to_string(v).unwrap_or_default().trim()))),
    }
}

/// Every entry of a top-level section (`networks:`, `volumes:`...).
fn each(v: &Value, mut f: impl FnMut(&str, &Value) -> Result<()>) -> Result<()> {
    for (k, entry) in v.as_mapping().into_iter().flatten() {
        f(key(k)?, plain(entry))?;
    }
    Ok(())
}

/// Refuse keys of a mapping that are not in `keys` (null and other shapes are left to Compose).
fn only(v: &Value, keys: &[&str], what: &str) -> Result<()> {
    for k in v.as_mapping().into_iter().flatten().map(|(k, _)| k) {
        let k = key(k)?;
        if !k.starts_with("x-") && !keys.contains(&k) {
            return Err(privileged(format_args!("{k} in {what}")));
        }
    }
    Ok(())
}

fn key(k: &Value) -> Result<&str> {
    plain(k).as_str().ok_or_else(|| anyhow!("the compose file has a key that is not a string"))
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).map(plain).and_then(Value::as_str)
}

/// A list, or a single value standing for a one-item list.
fn list(v: &Value) -> Vec<&Value> {
    match v {
        Value::Sequence(items) => items.iter().collect(),
        Value::Null => vec![],
        other => vec![other],
    }
}

/// The value without Compose's `!reset`/`!override` tags.
fn plain(v: &Value) -> &Value {
    match v {
        Value::Tagged(t) => plain(&t.value),
        other => other,
    }
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

pub async fn down(s: &Service, volumes: bool) -> Result<()> {
    let mut c = Command::new("docker");
    c.args(["compose", "-p", &s.slug(), "down", "--remove-orphans"]);
    if volumes {
        c.arg("--volumes");
    }
    crate::proc::output(c).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceSpec;

    fn stack() -> Service {
        Service {
            id: "svc1".into(),
            project_id: "p1".into(),
            project: "shop".into(),
            name: "plausible".into(),
            kind: "compose".into(),
            spec: ServiceSpec::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn refuses_host_access_without_privileged_mode() {
        let mut s = stack();
        let ok = "services:\n  web:\n    image: nginx\n    volumes: [\"./conf:/etc/nginx/conf.d:ro\", \"data:/data\", \"/cache\"]\n    build: { context: ., dockerfile: Dockerfile }\n    env_file: [.env.web]\n    security_opt: [\"no-new-privileges:true\"]\n    privileged: false\n    network_mode: \"service:db\"\n    labels: { tugport.replica: \"9\" }\n  db:\n    image: postgres\nvolumes:\n  data: {}\nnetworks:\n  back: { driver: bridge, internal: true }\nx-extra: 1\n";
        let out: Value = serde_yaml::from_str(&rewrite(ok, &s, "d1", "tp-shop").unwrap()).unwrap();
        assert!(out["services"]["web"]["labels"].get("tugport.replica").is_none(), "user labels cannot claim Tugport's");
        for bad in [
            "services:\n  web:\n    image: x\n    privileged: true\n",
            "services:\n  web:\n    image: x\n    privileged: !override true\n",
            "x-base: &b\n  privileged: true\nservices:\n  web:\n    <<: *b\n    image: x\n",
            "services:\n  web:\n    image: x\n    volumes: [\"/:/host\"]\n",
            "services:\n  web:\n    image: x\n    volumes: [\"../../etc:/x\"]\n",
            "services:\n  web:\n    image: x\n    volumes: [\"${DIR:-/}:/x\"]\n",
            "services:\n  web:\n    image: x\n    volumes: [{type: bind, source: /var/run/docker.sock, target: /s}]\n",
            "services:\n  web:\n    image: x\n    network_mode: host\n",
            "services:\n  web:\n    image: x\n    network_mode: \"container:abc\"\n",
            "services:\n  web:\n    image: x\n    pid: host\n",
            "services:\n  web:\n    image: x\n    cap_add: [SYS_ADMIN]\n",
            "services:\n  web:\n    image: x\n    devices: [\"/dev/sda:/dev/sda\"]\n",
            "services:\n  web:\n    image: x\n    use_api_socket: true\n",
            "services:\n  web:\n    provider: { type: sh }\n",
            "services:\n  web:\n    image: x\n    env_file: /etc/tugport/x\n",
            "services:\n  web:\n    image: x\n    volumes_from: [\"container:db\"]\n",
            "services:\n  web:\n    image: x\n    security_opt: [\"seccomp:unconfined\"]\n",
            "services:\n  web:\n    image: x\n    extends: { file: /etc/x.yml, service: a }\n",
            "services:\n  web:\n    build: { context: ., ssh: [default] }\n",
            "services:\n  web:\n    build: { context: ., network: host }\n",
            "services:\n  web:\n    build: { context: /etc }\n",
            "services:\n  web:\n    build: { context: ., cache_from: [\"type=local,src=/etc\"] }\n",
            "services:\n  web:\n    image: x\n    post_start: [{ command: id, privileged: true }]\n",
            "services:\n  web:\n    image: x\n    networks: [other]\nnetworks:\n  other: { external: true, name: tp-blog }\n",
            "services:\n  web:\n    image: x\n    volumes: [\"db:/d\"]\nvolumes:\n  db: { external: true, name: tp-blog-db-data }\n",
            "services:\n  web:\n    image: x\nvolumes:\n  db: { driver_opts: { type: none, o: bind, device: /etc } }\n",
            "services:\n  web:\n    image: x\nnetworks:\n  n: { ipam: { config: [{ subnet: 10.99.5.0/24 }] } }\n",
            "services:\n  web:\n    image: x\nsecrets:\n  k: { file: /etc/tugport/master.key }\n",
            "include: [/etc/x.yml]\nservices:\n  web:\n    image: x\n",
        ] {
            assert!(rewrite(bad, &s, "d1", "tp-shop").is_err(), "{bad}");
        }
        s.spec.privileged = true;
        assert!(rewrite("services:\n  web:\n    image: x\n    network_mode: host\n", &s, "d1", "tp-shop").is_ok());
    }

    #[test]
    fn rewrites_compose() {
        let s = stack();
        let out = rewrite("services:\n  web:\n    image: nginx\n    labels: [\"x=y\"]\n  db:\n    image: postgres\n    networks: [internal]\nnetworks:\n  internal: {}\n", &s, "d1", "tp-shop").unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v["services"]["web"]["labels"]["x"].as_str(), Some("y"));
        assert_eq!(v["services"]["web"]["labels"]["tugport.service"].as_str(), Some("svc1"));
        assert_eq!(v["services"]["web"]["networks"]["tp-shop"]["aliases"][0].as_str(), Some("plausible-web"));
        assert!(v["services"]["db"]["networks"].as_mapping().unwrap().contains_key("internal"));
        assert_eq!(v["networks"]["tp-shop"]["external"].as_bool(), Some(true));
    }
}
