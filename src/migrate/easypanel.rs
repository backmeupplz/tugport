//! Reading Easypanel and mapping it onto Tugport.
//!
//! Easypanel keeps its state in LMDB (`/etc/easypanel/data`, values are superjson strings).
//! Instead of reimplementing the format we read it with Easypanel's own lmdb binding,
//! inside its running container (a consistent read-only snapshot), or with its image on a
//! copy of the file when the panel is stopped.

use super::{Cert, Cutover, GITHUB, NewDomain, NewProject, NewService, Old, Plan, tugport_name};
use crate::databases;
use crate::model::{BasicUser, Build, ComposeSpec, DatabaseSpec, Engine, Health, HealthKind, Mount, Port, Protocol, Redirect, Resources, ServiceSpec, Source, Strategy};
use anyhow::{Context, Result};
use bollard::Docker;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

const ROOT: &str = "/etc/easypanel";

const DUMP_JS: &str = r#"
const { open } = require('/app/node_modules/lmdb');
const db = open({ path: process.env.EP_DATA || '/etc/easypanel/data', readOnly: true });
const out = [];
for (const { key, value } of db.getRange()) {
  let v = value;
  try { v = JSON.parse(value).json; } catch (e) {}
  out.push([String(key), v]);
}
process.stdout.write(JSON.stringify(out));
"#;

/// Easypanel's version if it runs here: `/etc/easypanel` plus its `easypanel` swarm service.
pub async fn detect(docker: &Docker) -> Option<String> {
    if !std::path::Path::new(ROOT).is_dir() {
        return None;
    }
    let svc = docker.inspect_service("easypanel", None::<bollard::query_parameters::InspectServiceOptions>).await.ok()?;
    let image = svc.spec?.task_template?.container_spec?.image?;
    let image = image.split('@').next().unwrap_or(&image).to_string();
    Some(image.rsplit_once(':').map(|(_, tag)| tag.to_string()).unwrap_or_else(|| "latest".into()))
}

pub async fn plan(docker: &Docker) -> Result<Plan> {
    let mut swarm = HashMap::new();
    for s in docker.list_services(None::<bollard::query_parameters::ListServicesOptions>).await.context("listing swarm services")? {
        let spec = s.spec.unwrap_or_default();
        if let (Some(name), Some(image)) = (spec.name, spec.task_template.and_then(|t| t.container_spec).and_then(|c| c.image)) {
            swarm.insert(name, image);
        }
    }
    let running = super::labeled(docker, "com.docker.swarm.service.name=easypanel", false).await?;
    let mut cmd = tokio::process::Command::new("docker");
    match running.first() {
        Some(id) => cmd.args(["exec", "-i", id, "node", "-"]),
        // The panel is stopped: read a copy, with the same image.
        None => {
            let image = swarm.get("easypanel").cloned().unwrap_or_else(|| "easypanel/easypanel".into());
            cmd.args(["run", "--rm", "-i", "-v", "/etc/easypanel/data:/ep:ro", "-e", "EP_DATA=/tmp/ep", "--entrypoint", "sh", &image])
                .args(["-c", "mkdir -p /tmp/ep && cp /ep/data.mdb /tmp/ep/ && node -"])
        }
    };
    let out = super::pipe(cmd, DUMP_JS).await.context("reading Easypanel's state")?;
    let entries: Vec<(String, Value)> = serde_json::from_slice(&out).context("reading Easypanel's state")?;
    let read = |p: &str| std::fs::read_to_string(format!("{ROOT}/{p}")).ok();
    let ctx = Ctx {
        acme: read("traefik/acme.json"),
        traefik: read("traefik/config/main.yaml"),
        swarm,
        exists: &|p: &str| std::path::Path::new(p).exists(),
    };
    Ok(map(entries, &ctx))
}

pub struct Ctx<'a> {
    pub acme: Option<String>,
    /// Traefik's dynamic config, to spot routes added by hand.
    pub traefik: Option<String>,
    /// Swarm service name -> the image it runs.
    pub swarm: HashMap<String, String>,
    pub exists: &'a dyn Fn(&str) -> bool,
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("").trim()
}

fn list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

pub fn map(entries: Vec<(String, Value)>, ctx: &Ctx) -> Plan {
    let mut settings: HashMap<String, Value> = HashMap::new();
    let (mut projects, mut services, mut domains) = (vec![], vec![], vec![]);
    let mut version = String::new();
    let mut other: BTreeMap<String, usize> = BTreeMap::new();
    for (key, value) in entries {
        let (prefix, rest) = key.split_once(':').unwrap_or((key.as_str(), ""));
        match prefix {
            "settings" => drop(settings.insert(rest.to_string(), value)),
            "projects" => projects.push(value),
            "services" => services.push(value),
            "domains" => domains.push(value),
            "version" => version = value.as_str().unwrap_or("").to_string(),
            _ => *other.entry(prefix.to_string()).or_default() += 1,
        }
    }
    let setting = |k: &str| settings.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let default_domain = setting("defaultDomain").unwrap_or_default();
    let service_domain = setting("customServiceDomain").unwrap_or_else(|| default_domain.clone());
    let github_token = setting("githubToken");
    let mut plan = Plan {
        panel: "easypanel",
        version: if version.is_empty() { "?".into() } else { version },
        github_token: github_token.clone(),
        acme_email: setting("letsEncryptEmail"),
        panel_domain: setting("customPanelDomain"),
        legacy_listen: Some("0.0.0.0:3000".into()),
        proxy: Old::Swarm { service: "easypanel-traefik".into() },
        old_panel: Old::Swarm { service: "easypanel".into() },
        ..Default::default()
    };
    let known: HashSet<String> = projects.iter().map(|p| st(p, "name").to_string()).collect();
    for p in &projects {
        plan.projects.push(NewProject { name: tugport_name(st(p, "name")), vars: dotenv(st(p, "env")) });
    }
    // Before 2.38 domains lived on the service records.
    let legacy_domains = domains.is_empty();
    for svc in &services {
        let (p, s) = (st(svc, "projectName"), st(svc, "name"));
        let placeholders = |text: &str| {
            let dashed = format!("{p}_{s}").replace('_', "-");
            text.replace("$(EASYPANEL_HOST)", &default_domain)
                .replace("$(EASYPANEL_DOMAIN)", &format!("{dashed}.{service_domain}"))
                .replace("$(PROJECT_NAME)", p)
                .replace("$(SERVICE_NAME)", s)
        };
        let mut n = NewService {
            key: format!("{p}/{s}"),
            project: tugport_name(p),
            name: tugport_name(s),
            kind: "app",
            spec: ServiceSpec { no_new_privileges: false, ..Default::default() },
            env: BTreeMap::new(),
            files: vec![],
            domains: vec![],
            hook: Some(st(svc, "token").to_string()).filter(|t| !t.is_empty()),
            image: ctx.swarm.get(&format!("{p}_{s}")).cloned(),
            commit: svc["commit"]["sha"].as_str().map(String::from),
            old: Old::Swarm { service: format!("{p}_{s}") },
            cutover: if svc["enabled"].as_bool() == Some(false) { Cutover::Idle } else { Cutover::Parallel },
            overlays: vec![],
            notes: vec![],
        };
        if !known.contains(p) {
            n.cutover = Cutover::Unsupported;
            n.notes.push("its project no longer exists in Easypanel".into());
            plan.services.push(n);
            continue;
        }
        let old_names = [format!("{p}_{s}"), format!("{p}_{s}").replace('_', "-")];
        n.spec.aliases = std::iter::once(s.to_string()).chain(old_names.iter().cloned()).filter(|a| *a != n.name).collect();
        n.spec.aliases.dedup();
        let mut overlay_aliases = old_names.to_vec();
        overlay_aliases.push(s.to_string());
        n.overlays = vec![("easypanel".into(), old_names.to_vec()), (format!("easypanel-{p}"), overlay_aliases)];
        let mine: Vec<&Value> = if legacy_domains {
            svc["domains"].as_array().map(|a| a.iter().collect()).unwrap_or_default()
        } else {
            domains
                .iter()
                .filter(|d| st(d, "destinationType") == "service" && st(&d["serviceDestination"], "projectName") == p && st(&d["serviceDestination"], "serviceName") == s)
                .collect()
        };
        match st(svc, "type") {
            "app" => app(svc, &mut n, ctx, github_token.is_some(), &mut plan.registries),
            "postgres" | "mysql" | "mariadb" | "mongo" | "redis" => database(svc, &mut n),
            "compose" => compose(svc, &mut n, &placeholders),
            other => {
                n.cutover = Cutover::Unsupported;
                n.notes.push(format!("{other} services are not supported; it keeps running, but its domains stop working after the switch"));
            }
        }
        if n.cutover != Cutover::Unsupported {
            n.domains = map_domains(svc, &mine, &mut n.notes, &placeholders, legacy_domains);
            if st(svc, "type") == "app"
                && let Some(primary) = domains.iter().find(|d| st(d, "id") == st(svc, "primaryDomainId"))
                && let Some(port) = primary["serviceDestination"]["port"].as_u64()
            {
                n.env.entry("PORT".into()).or_insert_with(|| port.to_string());
            }
        }
        plan.services.push(n);
    }
    dedupe_names(&mut plan.services);
    super::link_projects(
        &mut plan.services,
        |s| s.key.split_once('/').map(|(p, s)| vec![format!("{p}_{s}"), format!("{p}_{s}").replace('_', "-")]).unwrap_or_default(),
        |s| {
            let mut t: Vec<&str> = s.env.values().map(String::as_str).collect();
            t.extend(s.spec.command.iter().flatten().map(String::as_str));
            t.extend(s.spec.entrypoint.iter().flatten().map(String::as_str));
            t.extend(s.files.iter().map(|(_, c)| c.as_str()));
            if let Source::Compose { yaml } = &s.spec.source {
                t.push(yaml);
            }
            t.join("\n")
        },
    );
    let mut hosts: HashSet<String> = plan.services.iter().flat_map(|s| s.domains.iter().map(|d| d.host.clone())).collect();
    hosts.extend(plan.panel_domain.clone());
    plan.certs = ctx.acme.as_deref().map(|a| certs(a, &hosts)).unwrap_or_default();
    instance_notes(&mut plan, ctx, &other, &hosts, &default_domain);
    plan
}

/// Two old names can map to one Tugport name (`my_app`, `my-app`): number the later ones.
fn dedupe_names(services: &mut [NewService]) {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for s in services.iter_mut().filter(|s| s.cutover != Cutover::Unsupported) {
        let base = s.name.clone();
        let mut i = 2;
        while !seen.insert((s.project.clone(), s.name.clone())) {
            s.name = format!("{}-{i}", base.chars().take(37).collect::<String>());
            i += 1;
        }
        if s.name != base {
            s.notes.push(format!("renamed to {} because {base} is taken", s.name));
            s.spec.aliases.retain(|a| *a != s.name);
        }
    }
}

fn resources(v: &Value) -> Resources {
    // Easypanel applies limits only when a memory limit is set.
    let mem = v["memoryLimit"].as_u64().filter(|m| *m > 0);
    Resources {
        cpus: mem.and(v["cpuLimit"].as_f64()).filter(|c| *c > 0.0),
        memory_mb: mem,
        memory_reservation_mb: mem.and(v["memoryReservation"].as_u64()).filter(|m| *m > 0),
    }
}

fn shell(spec: &mut ServiceSpec, command: &str) {
    if !command.is_empty() {
        spec.entrypoint = Some(vec!["/bin/sh".into()]);
        spec.command = Some(vec!["-c".into(), command.into()]);
    }
}

fn app(svc: &Value, n: &mut NewService, ctx: &Ctx, has_token: bool, registries: &mut Vec<(String, String, String)>) {
    let (p, s) = (st(svc, "projectName"), st(svc, "name"));
    n.env = dotenv(st(svc, "env"));
    let src = &svc["source"];
    let path = st(src, "path").trim_matches('/').to_string();
    let current = || n.image.clone().unwrap_or_default();
    n.spec.source = match st(src, "type") {
        "github" if has_token => Source::Git {
            provider: Some(GITHUB.into()),
            repo: format!("{}/{}", st(src, "owner"), st(src, "repo")),
            branch: st(src, "ref").into(),
            path: path.clone(),
            watch_paths: vec![],
            auto_deploy: src["autoDeploy"].as_bool().unwrap_or(false),
        },
        "github" => {
            n.notes.push("Easypanel has no GitHub token, so the repository is cloned without one".into());
            Source::Git { provider: None, repo: format!("https://github.com/{}/{}", st(src, "owner"), st(src, "repo")), branch: st(src, "ref").into(), path: path.clone(), watch_paths: vec![], auto_deploy: false }
        }
        // Pushes reach plain Git sources through the deploy webhook.
        "git" => Source::Git { provider: None, repo: st(src, "repo").into(), branch: st(src, "ref").into(), path: path.clone(), watch_paths: vec![], auto_deploy: false },
        "image" => {
            let image = st(src, "image").to_string();
            if !st(src, "username").is_empty() {
                registries.push((crate::runtime::registry_host(&image).to_string(), st(src, "username").into(), st(src, "password").into()));
            }
            Source::Image { image }
        }
        "dockerfile" => Source::Dockerfile { content: st(src, "dockerfile").into() },
        other => {
            n.notes.push(format!("the {other} source cannot be rebuilt by Tugport; the current image keeps running"));
            Source::Image { image: current() }
        }
    };
    let b = &svc["build"];
    let mut set = |k: &str, v: &str| {
        if !v.is_empty() {
            n.env.entry(k.into()).or_insert_with(|| v.into());
        }
    };
    n.spec.build = match st(b, "type") {
        "dockerfile" => Build::Dockerfile { path: Some(st(b, "file")).filter(|f| !f.is_empty()).unwrap_or("Dockerfile").into(), target: None },
        "nixpacks" => {
            for (k, f) in [("NIXPACKS_INSTALL_CMD", "installCommand"), ("NIXPACKS_BUILD_CMD", "buildCommand"), ("NIXPACKS_START_CMD", "startCommand"), ("NIXPACKS_PKGS", "nixPackages"), ("NIXPACKS_APT_PKGS", "aptPackages")] {
                set(k, st(b, f));
            }
            Build::Nixpacks
        }
        "railpack" => {
            for (k, f) in [
                ("RAILPACK_BUILD_CMD", "buildCommand"),
                ("RAILPACK_INSTALL_CMD", "installCommand"),
                ("RAILPACK_START_CMD", "startCommand"),
                ("RAILPACK_PACKAGES", "misePackages"),
                ("RAILPACK_BUILD_APT_PACKAGES", "buildAptPackages"),
                ("RAILPACK_DEPLOY_APT_PACKAGES", "deployAptPackages"),
            ] {
                set(k, st(b, f));
            }
            Build::Railpack
        }
        "buildpacks" => Build::Buildpacks { builder: Some(st(b, "buildpacksBuilder")).filter(|f| !f.is_empty()).unwrap_or("heroku/builder:24").into() },
        "heroku-buildpacks" => Build::Buildpacks { builder: "heroku/builder:24".into() },
        "paketo-buildpacks" => Build::Buildpacks { builder: "paketobuildpacks/builder-jammy-base".into() },
        // No build settings: Easypanel uses the Dockerfile if there is one, else Nixpacks.
        _ if (ctx.exists)(&format!("{ROOT}/projects/{p}/{s}/code/{path}/Dockerfile").replace("//", "/")) => Build::Dockerfile { path: "Dockerfile".into(), target: None },
        _ => Build::Nixpacks,
    };
    if n.env.keys().any(|k| k.starts_with("NIXPACKS_") || k.starts_with("RAILPACK_")) {
        n.notes.push("build commands and packages became NIXPACKS_*/RAILPACK_* variables".into());
    }
    if let Some(v) = b["railpackVersion"].as_str().filter(|v| !v.is_empty()) {
        n.notes.push(format!("built with Tugport's Railpack instead of {v} from the next deploy"));
    }
    let d = &svc["deploy"];
    n.spec.replicas = d["replicas"].as_u64().unwrap_or(1).max(1) as u32;
    shell(&mut n.spec, st(d, "command"));
    n.spec.cap_add = list(&d["capAdd"]);
    n.spec.cap_drop = list(&d["capDrop"]);
    n.spec.groups = list(&d["groups"]);
    if let Some(sys) = d["sysctls"].as_object() {
        n.spec.sysctls = sys.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect();
    }
    if d["zeroDowntime"].as_bool() == Some(false) {
        n.spec.rollout.strategy = Strategy::StopFirst;
    }
    for port in svc["ports"].as_array().into_iter().flatten() {
        if let (Some(target), Some(published)) = (port["target"].as_u64(), port["published"].as_u64()) {
            let protocol = if st(port, "protocol") == "udp" { Protocol::Udp } else { Protocol::Tcp };
            n.spec.ports.push(Port { container: target as u16, host: published as u16, protocol });
        }
    }
    for m in svc["mounts"].as_array().into_iter().flatten() {
        let target = st(m, "mountPath").to_string();
        match st(m, "type") {
            "volume" => n.spec.mounts.push(Mount::Volume { name: format!("{p}_{s}_{}", st(m, "name")), target, readonly: false }),
            "bind" if st(m, "hostPath").ends_with("docker.sock") => {
                n.spec.docker_socket = true;
                n.notes.push("has access to the Docker socket".into());
            }
            "bind" => n.spec.mounts.push(Mount::Bind { source: st(m, "hostPath").into(), target, readonly: false }),
            "file" => {
                n.files.push((target.clone(), m["content"].as_str().unwrap_or("").to_string()));
                n.spec.mounts.push(Mount::File { target });
            }
            _ => {}
        }
    }
    n.spec.resources = resources(&svc["resources"]);
    if !st(svc, "dotEnvPath").is_empty() {
        n.notes.push("Easypanel wrote a .env file into the build; Tugport passes the variables to the build and the container instead".into());
    }
    if svc["scripts"].as_array().is_some_and(|a| !a.is_empty()) {
        n.notes.push("scripts are not moved".into());
    }
    if n.cutover == Cutover::Parallel && (n.spec.rollout.strategy == Strategy::StopFirst || !n.spec.ports.is_empty()) {
        n.cutover = Cutover::Swap;
    }
}

fn database(svc: &Value, n: &mut NewService) {
    let (p, s) = (st(svc, "projectName"), st(svc, "name"));
    let base = format!("{ROOT}/projects/{p}/{s}");
    let (user, password, root, db) = (st(svc, "user"), st(svc, "password"), st(svc, "rootPassword"), st(svc, "databaseName"));
    let (engine, data, default_image, env): (Engine, &str, &str, Vec<(&str, &str)>) = match st(svc, "type") {
        "postgres" => (Engine::Postgres, "/var/lib/postgresql/data", "postgres:18", vec![
            ("POSTGRES_PASSWORD", password),
            ("POSTGRES_USER", user),
            ("POSTGRES_DB", db),
            ("PGDATA", "/var/lib/postgresql/data"),
        ]),
        "mysql" => (Engine::Mysql, "/var/lib/mysql", "mysql:9", vec![("MYSQL_PASSWORD", password), ("MYSQL_ROOT_PASSWORD", root), ("MYSQL_DATABASE", db), ("MYSQL_USER", user)]),
        "mariadb" => (Engine::Mariadb, "/var/lib/mysql", "mariadb:11", vec![
            ("MARIADB_PASSWORD", password),
            ("MARIADB_ROOT_PASSWORD", root),
            ("MARIADB_DATABASE", db),
            ("MARIADB_USER", user),
        ]),
        "mongo" => (Engine::Mongo, "/data/db", "mongo:8", vec![("MONGO_INITDB_ROOT_USERNAME", user), ("MONGO_INITDB_ROOT_PASSWORD", password)]),
        _ => (Engine::Redis, "/data", "redis:7", vec![("REDIS_PASSWORD", password)]),
    };
    n.kind = "database";
    n.env = env.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    n.env.extend(dotenv(st(svc, "env")));
    let image = Some(st(svc, "image")).filter(|i| !i.is_empty()).unwrap_or(default_image);
    n.spec.source = Source::Image { image: image.into() };
    n.spec.mounts.push(Mount::Bind { source: format!("{base}/data"), target: data.into(), readonly: false });
    if matches!(engine, Engine::Mysql | Engine::Mariadb) {
        n.spec.mounts.push(Mount::Bind { source: format!("{base}/easypanel.cnf"), target: "/etc/mysql/conf.d/easypanel.cnf".into(), readonly: false });
    }
    if engine == Engine::Mysql {
        n.spec.cap_add = vec!["SYS_NICE".into()];
    }
    if engine == Engine::Redis {
        n.spec.entrypoint = Some(vec!["/bin/sh".into(), "-c".into(), "exec redis-server --requirepass \"$REDIS_PASSWORD\"".into()]);
    } else {
        shell(&mut n.spec, st(svc, "command"));
    }
    if let Some(port) = svc["exposedPort"].as_u64().filter(|p| *p > 0) {
        n.spec.ports.push(Port { container: databases::preset(engine).port, host: port as u16, protocol: Protocol::Tcp });
    }
    let check = match engine {
        // The official healthcheck.sh needs a user the old panel never created.
        Engine::Mariadb => "mariadb-admin ping -h 127.0.0.1 -uroot -p\"$MARIADB_ROOT_PASSWORD\" --silent",
        e => databases::health(e),
    };
    n.spec.health = Health { kind: HealthKind::Command, command: Some(check.into()), start_period_s: 30, ..Health::default() };
    n.spec.rollout.strategy = Strategy::StopFirst;
    n.spec.rollout.health_timeout_s = 600;
    n.spec.resources = resources(&svc["resources"]);
    n.spec.database = Some(DatabaseSpec { engine, user: user.into(), database: db.into(), replicas: vec![] });
    for tool in ["dbGate", "pgWeb", "mongoExpress", "phpMyAdmin", "redisCommander"] {
        if svc[tool]["enabled"].as_bool() == Some(true) {
            n.notes.push(format!("the {tool} admin tool is not moved"));
        }
    }
    if svc["backup"]["enabled"].as_bool() == Some(true) {
        n.notes.push("its backup schedule is not moved; set one up under Backups".into());
    }
    if n.cutover == Cutover::Parallel {
        n.cutover = Cutover::Swap;
    }
}

fn compose(svc: &Value, n: &mut NewService, placeholders: &dyn Fn(&str) -> String) {
    let (p, s) = (st(svc, "projectName"), st(svc, "name"));
    let src = &svc["source"];
    n.kind = "compose";
    n.env = dotenv(&placeholders(st(svc, "env")));
    n.old = Old::Labeled { label: format!("com.docker.compose.project={p}_{s}") };
    n.image = None;
    n.spec.compose = Some(ComposeSpec::default());
    n.spec.source = match st(src, "type") {
        "git" => {
            n.spec.compose = Some(ComposeSpec { file: Some(st(src, "composeFile")).filter(|f| !f.is_empty()).unwrap_or("docker-compose.yml").into() });
            n.notes.push(format!("named volumes in the repository's compose file start empty; the old data stays in volumes named {p}_{s}_*"));
            Source::Git { provider: None, repo: st(src, "repo").into(), branch: st(src, "ref").into(), path: st(src, "rootPath").trim_matches('/').into(), watch_paths: vec![], auto_deploy: false }
        }
        _ => {
            let yaml = placeholders(st(src, "content"));
            match keep_compose_data(&yaml, &format!("{p}_{s}"), &format!("{ROOT}/projects/{p}/{s}/code")) {
                Ok(y) => Source::Compose { yaml: y },
                Err(e) => {
                    n.notes.push(format!("the compose file could not be read ({e:#}); it moves as is"));
                    Source::Compose { yaml }
                }
            }
        }
    };
    if let Source::Compose { yaml } = &n.spec.source
        && yaml.contains("container_name")
    {
        n.notes.push("sets container_name, which clashes with the stopped old containers; remove them before moving".into());
    }
    // The old proxy reaches compose services as `<project>_<service>_<compose service>`.
    n.overlays = vec![("easypanel".into(), vec![format!("{p}_{s}_{{compose}}")])];
    if n.cutover == Cutover::Parallel {
        n.cutover = Cutover::Swap;
    }
}

/// Point the stack at the data the old one used: named volumes become the old project's
/// external volumes, relative bind mounts resolve against the old code directory.
pub fn keep_compose_data(yaml: &str, old_project: &str, code_dir: &str) -> Result<String> {
    use serde_yaml::{Mapping, Value as Y};
    let mut doc: Y = serde_yaml::from_str(yaml)?;
    if let Some(vols) = doc.get_mut("volumes").and_then(|v| v.as_mapping_mut()) {
        for (name, cfg) in vols.iter_mut() {
            let Some(name) = name.as_str() else { continue };
            if cfg.is_null() {
                *cfg = Y::Mapping(Mapping::new());
            }
            if let Some(m) = cfg.as_mapping_mut()
                && !m.contains_key("external")
                && !m.contains_key("name")
                && !m.contains_key("driver_opts")
            {
                m.insert("external".into(), true.into());
                m.insert("name".into(), format!("{old_project}_{name}").into());
            }
        }
    }
    let absolute = |src: &str| if src.starts_with('.') { format!("{code_dir}/{}", src.trim_start_matches("./")) } else { src.to_string() };
    if let Some(services) = doc.get_mut("services").and_then(|v| v.as_mapping_mut()) {
        for (_, svc) in services.iter_mut() {
            for v in svc.get_mut("volumes").and_then(|v| v.as_sequence_mut()).into_iter().flatten() {
                match v {
                    Y::String(s) if s.starts_with('.') => *s = absolute(s),
                    Y::Mapping(m) => {
                        if let Some(Y::String(src)) = m.get_mut("source")
                            && src.starts_with('.')
                        {
                            *src = absolute(src);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(serde_yaml::to_string(&doc)?)
}

fn map_domains(svc: &Value, mine: &[&Value], notes: &mut Vec<String>, placeholders: &dyn Fn(&str) -> String, legacy: bool) -> Vec<NewDomain> {
    let mut options = crate::model::DomainOptions::default();
    for u in svc["basicAuth"].as_array().into_iter().flatten() {
        match crate::auth::hash_password(st(u, "password")) {
            Ok(hash) => options.basic_auth.push(BasicUser { user: st(u, "username").into(), hash, password: None }),
            Err(e) => notes.push(format!("basic auth user {} was not moved: {e:#}", st(u, "username"))),
        }
    }
    for r in svc["redirects"].as_array().into_iter().flatten().filter(|r| r["enabled"].as_bool() != Some(false)) {
        options.redirects.push(Redirect { from: st(r, "regex").into(), to: st(r, "replacement").into(), permanent: r["permanent"].as_bool().unwrap_or(false) });
    }
    if svc["maintenance"]["enabled"].as_bool() == Some(true) {
        let m = &svc["maintenance"];
        let esc = |t: &str| t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        options.maintenance = Some(format!(
            "<!doctype html><meta charset=utf-8><title>{0}</title><main style=\"font:16px system-ui;text-align:center;margin-top:20vh\"><h1>{0}</h1><p>{1}</p></main>",
            esc(Some(st(m, "title")).filter(|t| !t.is_empty()).unwrap_or("Under maintenance")),
            esc(st(m, "subtitle"))
        ));
    }
    let mut out = vec![];
    for d in mine {
        let dest = if legacy { *d } else { &d["serviceDestination"] };
        let host = placeholders(st(d, "host")).to_lowercase();
        if d["wildcard"].as_bool() == Some(true) {
            notes.push(format!("the wildcard domain *.{host} is not supported"));
            continue;
        }
        let protocol = if legacy { st(d, "internalProtocol") } else { st(dest, "protocol") };
        if protocol == "https" {
            notes.push(format!("{host} talks HTTPS to the container; Tugport's proxy talks HTTP"));
        }
        let inner = st(dest, "path");
        if !legacy && !inner.is_empty() && inner != "/" {
            notes.push(format!("{host} forwards to the container path {inner}, which is not moved"));
        }
        if d["middlewares"].as_array().is_some_and(|m| !m.is_empty()) {
            notes.push(format!("{host} uses Traefik middlewares, which are not moved"));
        }
        let path = Some(st(d, "path")).filter(|p| !p.is_empty()).unwrap_or("/");
        out.push(NewDomain {
            host,
            path: path.into(),
            port: dest["port"].as_u64().unwrap_or(80) as u16,
            container: Some(st(dest, "composeService")).filter(|c| !c.is_empty()).map(String::from),
            https: d["https"].as_bool().unwrap_or(true),
            options: options.clone(),
        });
    }
    out
}

/// Certificates Traefik obtained, for the hosts that move.
pub fn certs(acme: &str, hosts: &HashSet<String>) -> Vec<Cert> {
    use base64::Engine as _;
    let Ok(v) = serde_json::from_str::<Value>(acme) else { return vec![] };
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).ok().and_then(|b| String::from_utf8(b).ok());
    let mut best: BTreeMap<String, (Option<i64>, Cert)> = BTreeMap::new();
    for resolver in v.as_object().into_iter().flat_map(|o| o.values()) {
        for c in resolver["Certificates"].as_array().into_iter().flatten() {
            let (Some(cert), Some(key)) = (b64(st(c, "certificate")), b64(st(c, "key"))) else { continue };
            let names = std::iter::once(st(&c["domain"], "main").to_string()).chain(list(&c["domain"]["sans"]));
            for host in names.map(|h| h.to_lowercase()).filter(|h| hosts.contains(h)) {
                let not_after = crate::acme::first_cert_not_after(&cert);
                if best.get(&host).is_none_or(|(have, _)| not_after > *have) {
                    best.insert(host.clone(), (not_after, Cert { host, cert_pem: cert.clone(), key_pem: key.clone() }));
                }
            }
        }
    }
    best.into_values().map(|(_, c)| c).collect()
}

fn instance_notes(plan: &mut Plan, ctx: &Ctx, other: &BTreeMap<String, usize>, hosts: &HashSet<String>, default_domain: &str) {
    for (kind, what) in [
        ("databaseBackups", "database backup schedules"),
        ("volumeBackups", "volume backup schedules"),
        ("notificationChannels", "notification channels"),
        ("middlewares", "custom Traefik middlewares"),
        ("cloudflareTunnelRules", "Cloudflare Tunnel rules"),
    ] {
        if let Some(n) = other.get(kind) {
            plan.notes.push(format!("{n} {what}"));
        }
    }
    static HOST: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"Host\(`([^`]+)`\)").ok());
    if let (Some(cfg), Some(re)) = (ctx.traefik.as_deref().and_then(|t| serde_json::from_str::<Value>(t).ok()), HOST.as_ref()) {
        let mut manual: Vec<String> = cfg["http"]["routers"]
            .as_object()
            .into_iter()
            .flat_map(|r| r.values())
            .flat_map(|r| re.captures_iter(st(r, "rule")).map(|c| c[1].to_lowercase()).collect::<Vec<_>>())
            .filter(|h| !hosts.contains(h) && h != default_domain && !h.ends_with(&format!(".{default_domain}")))
            .collect();
        manual.sort();
        manual.dedup();
        if !manual.is_empty() {
            plan.notes.push(format!("Traefik routes Easypanel does not manage (added by hand?): {}", manual.join(", ")));
        }
    }
    let managed: HashSet<String> = plan.services.iter().filter_map(|s| s.key.split_once('/').map(|(p, s)| format!("{p}_{s}"))).collect();
    let mut loose: Vec<&String> = ctx.swarm.keys().filter(|k| !managed.contains(*k) && !k.starts_with("easypanel")).collect();
    loose.sort();
    if !loose.is_empty() {
        plan.notes.push(format!(
            "swarm services Easypanel does not manage keep running untouched, but moved services cannot reach them by name: {}",
            loose.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
}

/// `dotenv.parse` as Easypanel runs it (dotenv 16): `KEY=value` lines, quotes, `export`,
/// comments; `\n` in double quotes becomes a newline.
pub fn dotenv(src: &str) -> BTreeMap<String, String> {
    static LINE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r#"(?m)^\s*(?:export\s+)?([A-Za-z0-9_.-]+)(?:\s*=\s*?|:\s+?)(\s*'(?:\\'|[^'])*'|\s*"(?:\\"|[^"])*"|\s*`(?:\\`|[^`])*`|[^#\r\n]+)?\s*(?:#.*)?$"#).ok()
    });
    let mut out = BTreeMap::new();
    let Some(re) = LINE.as_ref() else { return out };
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    for c in re.captures_iter(&src) {
        let mut v = c.get(2).map(|m| m.as_str()).unwrap_or("").trim().to_string();
        let quote = v.chars().next();
        if let Some(q @ ('\'' | '"' | '`')) = quote
            && v.len() >= 2
            && v.ends_with(q)
        {
            v = v[1..v.len() - 1].to_string();
        }
        if quote == Some('"') {
            v = v.replace("\\n", "\n").replace("\\r", "\r");
        }
        out.insert(c[1].to_string(), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_dotenv_like_easypanel() {
        let env = dotenv("A=1\nexport B = two words \n# c\nC=\"multi\\nline\" # note\nD='single # kept'\nE=\nF.G-H=x\r\nURL=postgres://u:p@h:5432/db?x=1#frag\n");
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "two words");
        assert_eq!(env["C"], "multi\nline");
        assert_eq!(env["D"], "single # kept");
        assert_eq!(env["E"], "");
        assert_eq!(env["F.G-H"], "x");
        assert_eq!(env["URL"], "postgres://u:p@h:5432/db?x=1");
        assert!(!env.contains_key("c"));
    }

    #[test]
    fn keeps_compose_data() {
        let out = keep_compose_data("services:\n  web:\n    image: x\n    volumes: [\"state:/s\", \"./conf:/c:ro\"]\nvolumes:\n  state:\n  ext:\n    external: true\n", "blog_stack", "/ep/code").unwrap();
        let y: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(y["volumes"]["state"]["name"].as_str(), Some("blog_stack_state"));
        assert_eq!(y["volumes"]["state"]["external"].as_bool(), Some(true));
        assert!(y["volumes"]["ext"].get("name").is_none());
        assert_eq!(y["services"]["web"]["volumes"][1].as_str(), Some("/ep/code/conf:/c:ro"));
    }

    fn fixture() -> Vec<(String, Value)> {
        vec![
            ("version".into(), json!("2.38.0")),
            ("settings:githubToken".into(), json!("ghp_test")),
            ("settings:letsEncryptEmail".into(), json!("ops@example.com")),
            ("settings:defaultDomain".into(), json!("abc.easypanel.host")),
            ("projects:shop".into(), json!({"name": "shop", "env": "SHARED=1"})),
            ("projects:bots".into(), json!({"name": "bots"})),
            ("services:shop:api".into(), json!({
                "type": "app", "projectName": "shop", "name": "api", "enabled": true, "token": "tok1",
                "env": "DB=postgres://u:p@shop_db:5432/shop\nMONGO=mongodb://bots_store:27017", "primaryDomainId": "d1",
                "source": {"type": "github", "owner": "o", "repo": "r", "ref": "main", "path": "/", "autoDeploy": true},
                "build": {"type": "nixpacks", "startCommand": "npm start", "nixpacksVersion": "1.41.0"},
                "deploy": {"replicas": 1, "command": null, "zeroDowntime": true},
                "mounts": [{"type": "volume", "name": "data", "mountPath": "/data"}, {"type": "file", "content": "x=1", "mountPath": "/etc/app.conf"}],
                "ports": [], "basicAuth": [{"username": "u", "password": "pw"}], "commit": {"sha": "abc"}
            })),
            ("services:shop:db".into(), json!({"type": "postgres", "projectName": "shop", "name": "db", "enabled": true, "image": "postgres:16", "user": "u", "password": "p", "databaseName": "shop", "exposedPort": 0})),
            ("services:bots:cas_ban_bot".into(), json!({
                "type": "app", "projectName": "bots", "name": "cas_ban_bot", "enabled": true,
                "source": {"type": "image", "image": "ghcr.io/o/bot:1", "username": "u", "password": "p"},
                "deploy": {"replicas": 1, "zeroDowntime": false, "command": "node bot.js"}, "env": "", "mounts": [], "ports": []
            })),
            ("services:bots:store".into(), json!({"type": "redis", "projectName": "bots", "name": "store", "enabled": true, "password": "rp", "image": "redis:7"})),
            ("services:bots:box".into(), json!({"type": "box", "projectName": "bots", "name": "box"})),
            ("domains:d1".into(), json!({
                "id": "d1", "host": "api.example.com", "https": true, "path": "/", "wildcard": false, "middlewares": [], "destinationType": "service",
                "serviceDestination": {"projectName": "shop", "serviceName": "api", "port": 3000, "path": "/", "protocol": "http"}
            })),
            ("databaseBackups:x".into(), json!({})),
        ]
    }

    #[test]
    fn maps_a_store() {
        let mut swarm = HashMap::new();
        swarm.insert("shop_api".to_string(), "easypanel/shop/api:latest".to_string());
        swarm.insert("loose_thing".to_string(), "nginx".to_string());
        let ctx = Ctx { acme: None, traefik: None, swarm, exists: &|_| false };
        let plan = map(fixture(), &ctx);
        assert_eq!(plan.version, "2.38.0");
        assert_eq!(plan.github_token.as_deref(), Some("ghp_test"));
        assert_eq!(plan.projects.iter().find(|p| p.name == "shop").unwrap().vars["SHARED"], "1");
        let get = |k: &str| plan.services.iter().find(|s| s.key == k).unwrap();

        let api = get("shop/api");
        assert_eq!(api.cutover, Cutover::Parallel);
        assert_eq!(api.image.as_deref(), Some("easypanel/shop/api:latest"));
        assert!(matches!(&api.spec.source, Source::Git { provider: Some(p), repo, auto_deploy: true, .. } if p == GITHUB && repo == "o/r"));
        assert_eq!(api.env["NIXPACKS_START_CMD"], "npm start");
        assert_eq!(api.env["PORT"], "3000");
        assert!(api.spec.mounts.contains(&Mount::Volume { name: "shop_api_data".into(), target: "/data".into(), readonly: false }));
        assert_eq!(api.files, vec![("/etc/app.conf".to_string(), "x=1".to_string())]);
        assert_eq!(api.domains[0].host, "api.example.com");
        assert!(crate::auth::verify_password("pw", &api.domains[0].options.basic_auth[0].hash));
        assert_eq!(api.hook.as_deref(), Some("tok1"));
        assert!(!api.spec.no_new_privileges);
        assert!(api.spec.aliases.contains(&"shop_api".to_string()));

        let db = get("shop/db");
        assert_eq!((db.kind, db.cutover), ("database", Cutover::Swap));
        assert_eq!(db.env["POSTGRES_DB"], "shop");
        assert!(db.spec.mounts.contains(&Mount::Bind { source: "/etc/easypanel/projects/shop/db/data".into(), target: "/var/lib/postgresql/data".into(), readonly: false }));
        assert_eq!(db.overlays[1], ("easypanel-shop".to_string(), vec!["shop_db".to_string(), "shop-db".to_string(), "db".to_string()]));

        let bot = get("bots/cas_ban_bot");
        assert_eq!((bot.name.as_str(), bot.cutover), ("cas-ban-bot", Cutover::Swap));
        assert!(bot.spec.aliases.contains(&"cas_ban_bot".to_string()) && bot.spec.aliases.contains(&"bots-cas-ban-bot".to_string()));
        assert_eq!(bot.spec.command, Some(vec!["-c".to_string(), "node bot.js".to_string()]));
        assert_eq!(plan.registries, vec![("ghcr.io".to_string(), "u".to_string(), "p".to_string())]);

        // shop/api reaches bots/store by its old name, so the store joins shop's network.
        assert_eq!(get("bots/store").spec.networks, vec!["tp-shop".to_string()]);
        assert_eq!(get("bots/box").cutover, Cutover::Unsupported);
        assert!(plan.notes.iter().any(|n| n.contains("database backup")));
        assert!(plan.notes.iter().any(|n| n.contains("loose_thing")));
    }
}
