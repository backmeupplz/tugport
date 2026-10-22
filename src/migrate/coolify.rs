//! Reading Coolify and mapping it onto Tugport.
//!
//! Coolify (Laravel) keeps its state in Postgres, in the `coolify-db` container. We query it
//! with `psql` inside that container (its local socket needs no password) and decrypt
//! Laravel's encrypted columns with `APP_KEY` from `/data/coolify/source/.env`. The
//! containers Coolify runs supply what actually ran: image, mounts, command and old names.

use super::{Cutover, NewDomain, NewProject, NewService, Old, Plan, tugport_name};
use crate::model::{BasicUser, Build, ComposeSpec, DatabaseSpec, Engine, Health, HealthKind, Mount, Port, Protocol, ServiceSpec, Source, Strategy, WwwRedirect};
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use bollard::Docker;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

const ENV_FILE: &str = "/data/coolify/source/.env";

/// Coolify's version if it runs here: its `.env` plus a `coolify` container.
pub async fn detect(docker: &Docker) -> Option<String> {
    if !std::path::Path::new(ENV_FILE).exists() {
        return None;
    }
    let c = docker.inspect_container("coolify", None::<bollard::query_parameters::InspectContainerOptions>).await.ok()?;
    let image = c.config?.image?;
    Some(image.rsplit_once(':').map(|(_, t)| t.to_string()).unwrap_or_else(|| "latest".into()))
}

const TABLES: &[&str] = &[
    "projects",
    "environments",
    "applications",
    "application_settings",
    "standalone_postgresqls",
    "standalone_mysqls",
    "standalone_mariadbs",
    "standalone_mongodbs",
    "standalone_redis",
    "standalone_keydbs",
    "standalone_dragonflies",
    "standalone_clickhouses",
    "services",
    "service_applications",
    "environment_variables",
    "shared_environment_variables",
    "local_persistent_volumes",
    "local_file_volumes",
    "standalone_dockers",
    "instance_settings",
];

pub async fn plan(docker: &Docker) -> Result<Plan> {
    let env = super::easypanel::dotenv(&std::fs::read_to_string(ENV_FILE).with_context(|| format!("reading {ENV_FILE}"))?);
    let key = app_key(env.get("APP_KEY").map(String::as_str).unwrap_or_default())?;
    let user = env.get("DB_USERNAME").map(String::as_str).unwrap_or("coolify");
    let db = env.get("DB_DATABASE").map(String::as_str).unwrap_or("coolify");
    let mut rows = HashMap::new();
    for t in TABLES {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(["exec", "-i", "coolify-db", "psql", "-U", user, "-d", db, "-At", "-v", "ON_ERROR_STOP=1"]);
        // A table this Coolify version lacks reads as empty.
        let out = super::pipe(cmd, &format!("select row_to_json(t) from (select * from {t}) t;")).await.unwrap_or_default();
        let list: Vec<Value> = String::from_utf8_lossy(&out).lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        rows.insert(t.to_string(), list.into_iter().filter(|r| r["deleted_at"].is_null()).collect());
    }
    let mut running = vec![];
    for id in super::labeled(docker, "coolify.managed=true", true).await? {
        let Ok(c) = docker.inspect_container(&id, None::<bollard::query_parameters::InspectContainerOptions>).await else { continue };
        let config = c.config.unwrap_or_default();
        let image_id = c.image.unwrap_or_default();
        let image_cmd = docker.inspect_image(&image_id).await.ok().and_then(|i| i.config?.cmd);
        let aliases = c.network_settings.and_then(|n| n.networks).map(|n| n.into_values().flat_map(|e| e.aliases.unwrap_or_default()).collect()).unwrap_or_default();
        let mounts = c
            .mounts
            .unwrap_or_default()
            .into_iter()
            .map(|m| {
                let kind = m.typ.map(|t| t.to_string()).unwrap_or_default();
                let source = if kind == "volume" { m.name } else { m.source };
                (kind, source.unwrap_or_default(), m.destination.unwrap_or_default(), m.rw.unwrap_or(true))
            })
            .collect();
        running.push(Running {
            name: c.name.unwrap_or_default().trim_start_matches('/').to_string(),
            labels: config.labels.unwrap_or_default(),
            image_id,
            env: config.env.unwrap_or_default(),
            cmd: config.cmd,
            image_cmd,
            mounts,
            aliases,
            exposed: config.exposed_ports.unwrap_or_default().keys().filter_map(|p| p.split('/').next()?.parse().ok()).collect(),
            running: c.state.and_then(|s| s.running).unwrap_or(false),
        });
    }
    let acme = std::fs::read_to_string("/data/coolify/proxy/acme.json").ok();
    let version = detect(docker).await.unwrap_or_else(|| "?".into());
    Ok(map(&rows, &running, &key, acme.as_deref(), &version))
}

/// What one of Coolify's containers runs.
#[derive(Default, Clone)]
pub struct Running {
    pub name: String,
    pub labels: HashMap<String, String>,
    pub image_id: String,
    pub env: Vec<String>,
    pub cmd: Option<Vec<String>>,
    pub image_cmd: Option<Vec<String>>,
    /// (type, volume name or host path, target, writable)
    pub mounts: Vec<(String, String, String, bool)>,
    pub aliases: Vec<String>,
    pub exposed: Vec<u16>,
    pub running: bool,
}

pub fn app_key(raw: &str) -> Result<[u8; 32]> {
    let raw = raw.trim();
    let bytes = match raw.strip_prefix("base64:") {
        Some(b) => base64::engine::general_purpose::STANDARD.decode(b)?,
        None => raw.as_bytes().to_vec(),
    };
    bytes.try_into().map_err(|_| anyhow!("APP_KEY must be 32 bytes (AES-256-CBC)"))
}

/// Laravel's `encrypt()`: base64 JSON `{iv, value, mac}`, AES-256-CBC, HMAC-SHA256 over
/// base64(iv) + base64(value). Values written by `encrypt()` (not `encryptString()`) are PHP
/// serialized strings, which are unwrapped.
pub fn decrypt(key: &[u8; 32], payload: &str) -> Result<String> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    use hmac::{Hmac, Mac};
    let b64 = base64::engine::general_purpose::STANDARD;
    let json: Value = serde_json::from_slice(&b64.decode(payload.trim()).context("not base64")?).context("not a Laravel payload")?;
    let field = |k: &str| json[k].as_str().unwrap_or_default().to_string();
    let (iv, value) = (field("iv"), field("value"));
    let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).map_err(|_| anyhow!("bad key"))?;
    mac.update(iv.as_bytes());
    mac.update(value.as_bytes());
    mac.verify_slice(&hex::decode(field("mac")).context("bad MAC")?).map_err(|_| anyhow!("the MAC does not match; is APP_KEY right?"))?;
    let plain = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, &b64.decode(iv)?)
        .map_err(|_| anyhow!("bad IV"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&b64.decode(value)?)
        .map_err(|_| anyhow!("bad padding"))?;
    let text = String::from_utf8(plain)?;
    Ok(unserialize(&text).unwrap_or(text))
}

/// PHP's serialized string `s:5:"hello";`.
fn unserialize(s: &str) -> Option<String> {
    let (len, rest) = s.strip_prefix("s:")?.split_once(':')?;
    let len: usize = len.parse().ok()?;
    let body = rest.strip_prefix('"')?.strip_suffix("\";")?;
    (body.len() == len).then(|| body.to_string())
}

fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("").trim()
}

fn id(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap_or(-1)
}

struct Ctx<'a> {
    rows: &'a HashMap<String, Vec<Value>>,
    running: &'a [Running],
    key: &'a [u8; 32],
}

impl Ctx<'_> {
    fn table(&self, t: &str) -> &[Value] {
        self.rows.get(t).map(|v| v.as_slice()).unwrap_or_default()
    }
    fn secret(&self, v: &Value, k: &str, notes: &mut Vec<String>) -> String {
        match v[k].as_str().filter(|s| !s.is_empty()) {
            Some(p) => decrypt(self.key, p).unwrap_or_else(|e| {
                notes.push(format!("{k} could not be decrypted: {e:#}"));
                String::new()
            }),
            None => String::new(),
        }
    }
    /// Containers of a resource (`coolify.applicationUuid=...` and friends), running first.
    fn containers(&self, label: &str, uuid: &str) -> Vec<&Running> {
        let mut v: Vec<&Running> = self.running.iter().filter(|r| r.labels.get(label).is_some_and(|u| u == uuid)).collect();
        v.sort_by_key(|r| !r.running);
        v
    }
    /// The resource's variables with `{{project.X}}`-style shared variables resolved.
    fn env(&self, kind: &str, resource_id: i64, scope: (i64, i64, i64), notes: &mut Vec<String>) -> (BTreeMap<String, String>, usize) {
        let (env_id, project_id, team_id) = scope;
        let mut out = BTreeMap::new();
        let mut build_only = 0;
        for e in self.table("environment_variables") {
            if st(e, "resourceable_type") != kind || id(e, "resourceable_id") != resource_id || e["is_preview"].as_bool() == Some(true) {
                continue;
            }
            let raw = self.secret(e, "value", notes);
            let value = SHARED.as_ref().map(|re| {
                re.replace_all(raw.trim(), |c: &regex::Captures| {
                    let (scope, key) = (&c[1], &c[2]);
                    let want = match scope {
                        "environment" => ("environment_id", env_id),
                        "project" => ("project_id", project_id),
                        "team" => ("team_id", team_id),
                        _ => ("server_id", 0),
                    };
                    self.table("shared_environment_variables")
                        .iter()
                        .find(|s| st(s, "type") == scope && st(s, "key") == key && id(s, want.0) == want.1)
                        .map(|s| self.secret(s, "value", &mut vec![]))
                        .unwrap_or_else(|| c[0].to_string())
                })
                .to_string()
            });
            if e["is_runtime"].as_bool() == Some(false) {
                build_only += 1;
            }
            out.insert(st(e, "key").to_string(), value.unwrap_or(raw));
        }
        (out, build_only)
    }
}

static SHARED: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"\{\{\s*(environment|project|team|server)\.([^}\s]+)\s*\}\}").ok());

pub fn map(rows: &HashMap<String, Vec<Value>>, running: &[Running], key: &[u8; 32], acme: Option<&str>, version: &str) -> Plan {
    let ctx = Ctx { rows, running, key };
    let mut plan = Plan {
        panel: "coolify",
        version: version.into(),
        proxy: Old::Labeled { label: "coolify.proxy=true".into() },
        old_panel: Old::Container { name: "coolify".into() },
        ..Default::default()
    };
    let projects: HashMap<i64, &Value> = ctx.table("projects").iter().map(|p| (id(p, "id"), p)).collect();
    // Environment id -> (Tugport project, project id, team id). Production keeps the project's name.
    let mut envs: HashMap<i64, (String, i64, i64)> = HashMap::new();
    for e in ctx.table("environments") {
        let Some(p) = projects.get(&id(e, "project_id")) else { continue };
        let name = if st(e, "name") == "production" { tugport_name(st(p, "name")) } else { tugport_name(&format!("{}-{}", st(p, "name"), st(e, "name"))) };
        envs.insert(id(e, "id"), (name.clone(), id(p, "id"), id(p, "team_id")));
        if !plan.projects.iter().any(|x| x.name == name) {
            plan.projects.push(NewProject { name, vars: BTreeMap::new() });
        }
    }
    let networks: HashMap<i64, String> = ctx.table("standalone_dockers").iter().map(|d| (id(d, "id"), st(d, "network").to_string())).collect();
    let settings: HashMap<i64, &Value> = ctx.table("application_settings").iter().map(|s| (id(s, "application_id"), s)).collect();

    for a in ctx.table("applications") {
        let Some((project, pid, team)) = envs.get(&id(a, "environment_id")).cloned() else { continue };
        let set = settings.get(&id(a, "id")).copied().unwrap_or(&Value::Null);
        let uuid = st(a, "uuid");
        let mut n = base(&project, st(a, "name"), uuid);
        let containers = ctx.containers("coolify.applicationUuid", uuid);
        let (env, build_only) = ctx.env("App\\Models\\Application", id(a, "id"), (id(a, "environment_id"), pid, team), &mut n.notes);
        n.env = env;
        if build_only > 0 {
            n.notes.push(format!("{build_only} build-time-only variables are also set when the app runs"));
        }
        fill_from_containers(&mut n, &containers);
        if n.spec.mounts.is_empty() {
            n.spec.mounts = stored_volumes(&ctx, "Application", id(a, "id"));
        }
        n.old = Old::Labeled { label: format!("coolify.applicationUuid={uuid}") };
        let mut names = vec![uuid.to_string()];
        names.extend(st(a, "custom_network_aliases").split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from));
        names.extend(Some(st(set, "custom_internal_name")).filter(|s| !s.is_empty()).map(String::from));
        names.extend(containers.iter().flat_map(|c| std::iter::once(c.name.clone()).chain(c.aliases.iter().cloned())));
        set_names(&mut n, names, networks.get(&id(a, "destination_id")));
        let image = format!("{}:{}", st(a, "docker_registry_image_name"), Some(st(a, "docker_registry_image_tag")).filter(|t| !t.is_empty()).unwrap_or("latest"));
        let repo = st(a, "git_repository");
        let git = |n: &mut NewService| {
            let public = st(a, "source_type").is_empty() || (st(a, "source_type").ends_with("GithubApp") && id(a, "source_id") == 0);
            let repo = if repo.contains("://") || repo.starts_with("git@") || !public { repo.to_string() } else { format!("https://github.com/{repo}") };
            if !public {
                n.notes.push("the repository is private through Coolify's GitHub App; connect GitHub in Tugport and pick it again".into());
            }
            if !a["private_key_id"].is_null() {
                n.notes.push("the repository uses a Coolify deploy key; add a deploy key in Tugport".into());
            }
            Source::Git { provider: None, repo, branch: st(a, "git_branch").into(), path: st(a, "base_directory").trim_matches('/').into(), watch_paths: vec![], auto_deploy: false }
        };
        let set_cmd = |n: &mut NewService, pre: &str| {
            for (k, f) in [("INSTALL_CMD", "install_command"), ("BUILD_CMD", "build_command"), ("START_CMD", "start_command")] {
                if !st(a, f).is_empty() {
                    n.env.entry(format!("{pre}_{k}")).or_insert_with(|| st(a, f).to_string());
                }
            }
        };
        match st(a, "build_pack") {
            "dockerimage" => n.spec.source = Source::Image { image },
            "dockerfile" if !st(a, "dockerfile").is_empty() => n.spec.source = Source::Dockerfile { content: st(a, "dockerfile").into() },
            "dockerfile" => {
                n.spec.source = git(&mut n);
                n.spec.build = Build::Dockerfile { path: Some(st(a, "dockerfile_location").trim_start_matches('/')).filter(|p| !p.is_empty()).unwrap_or("Dockerfile").into(), target: None };
            }
            "dockercompose" => {
                n.kind = "compose";
                n.spec.source = git(&mut n);
                n.spec.compose = Some(ComposeSpec { file: Some(st(a, "docker_compose_location").trim_start_matches('/')).filter(|p| !p.is_empty()).unwrap_or("docker-compose.yml").into() });
                n.notes.push("compose apps from Git start with empty named volumes; the old data stays in Coolify's volumes".into());
            }
            "railpack" => {
                n.spec.source = git(&mut n);
                n.spec.build = Build::Railpack;
                set_cmd(&mut n, "RAILPACK");
            }
            other => {
                n.spec.source = git(&mut n);
                n.spec.build = Build::Nixpacks;
                set_cmd(&mut n, "NIXPACKS");
                if other == "static" {
                    n.notes.push("the static build pack is built with Nixpacks".into());
                }
            }
        }
        if let Source::Git { auto_deploy, .. } = &mut n.spec.source {
            *auto_deploy = false;
        }
        let ports = st(a, "ports_exposes").split(',').filter_map(|p| p.trim().parse().ok()).collect::<Vec<u16>>();
        n.spec.ports = port_mappings(st(a, "ports_mappings"), &mut n.notes);
        let mut options = crate::model::DomainOptions {
            www_redirect: match st(a, "redirect") {
                "www" => Some(WwwRedirect::ToWww),
                "non-www" => Some(WwwRedirect::ToApex),
                _ => None,
            },
            ..Default::default()
        };
        if a["is_http_basic_auth_enabled"].as_bool() == Some(true) {
            let pw = ctx.secret(a, "http_basic_auth_password", &mut n.notes);
            match crate::auth::hash_password(&pw) {
                Ok(hash) => options.basic_auth.push(BasicUser { user: st(a, "http_basic_auth_username").into(), hash }),
                Err(e) => n.notes.push(format!("basic auth was not moved: {e:#}")),
            }
        }
        n.domains = domains(st(a, "fqdn"), ports.first().copied().unwrap_or(80), None, &options);
        n.spec.resources = limits(a);
        if a["health_check_enabled"].as_bool() == Some(true) {
            n.spec.health = Health {
                kind: HealthKind::Http,
                path: Some(st(a, "health_check_path")).filter(|p| !p.is_empty()).unwrap_or("/").into(),
                port: st(a, "health_check_port").parse().ok().or(ports.first().copied()),
                ..Health::default()
            };
        }
        if !st(a, "custom_docker_run_options").is_empty() {
            n.notes.push("custom docker run options are not moved".into());
        }
        n.cutover = if !st(a, "status").starts_with("running") {
            Cutover::Idle
        } else if !n.spec.ports.is_empty() || n.kind == "compose" {
            Cutover::Swap
        } else {
            Cutover::Parallel
        };
        plan.services.push(n);
    }

    for (table, engine) in [
        ("standalone_postgresqls", Some(Engine::Postgres)),
        ("standalone_mysqls", Some(Engine::Mysql)),
        ("standalone_mariadbs", Some(Engine::Mariadb)),
        ("standalone_mongodbs", Some(Engine::Mongo)),
        ("standalone_redis", Some(Engine::Redis)),
        ("standalone_keydbs", None),
        ("standalone_dragonflies", None),
        ("standalone_clickhouses", None),
    ] {
        for d in ctx.table(table) {
            let Some((project, pid, team)) = envs.get(&id(d, "environment_id")).cloned() else { continue };
            let uuid = st(d, "uuid");
            let mut n = base(&project, st(d, "name"), uuid);
            let Some(engine) = engine else {
                n.cutover = Cutover::Unsupported;
                n.notes.push(format!("{table} are not supported; it keeps running, but Tugport's services cannot reach it after the move"));
                plan.services.push(n);
                continue;
            };
            let model = match engine {
                Engine::Postgres => "StandalonePostgresql",
                Engine::Mysql => "StandaloneMysql",
                Engine::Mariadb => "StandaloneMariadb",
                Engine::Mongo => "StandaloneMongodb",
                Engine::Redis => "StandaloneRedis",
            };
            let (mut env, _) = ctx.env(&format!("App\\Models\\{model}"), id(d, "id"), (id(d, "environment_id"), pid, team), &mut n.notes);
            let mut put = |k: &str, v: String| {
                if !v.is_empty() {
                    env.entry(k.into()).or_insert(v);
                }
            };
            let (user, database) = match engine {
                Engine::Postgres => {
                    put("POSTGRES_USER", st(d, "postgres_user").into());
                    put("PGUSER", st(d, "postgres_user").into());
                    put("POSTGRES_PASSWORD", ctx.secret(d, "postgres_password", &mut n.notes));
                    put("POSTGRES_DB", st(d, "postgres_db").into());
                    (st(d, "postgres_user"), st(d, "postgres_db"))
                }
                Engine::Mysql => {
                    put("MYSQL_ROOT_PASSWORD", ctx.secret(d, "mysql_root_password", &mut n.notes));
                    put("MYSQL_DATABASE", st(d, "mysql_database").into());
                    put("MYSQL_USER", st(d, "mysql_user").into());
                    put("MYSQL_PASSWORD", ctx.secret(d, "mysql_password", &mut n.notes));
                    (st(d, "mysql_user"), st(d, "mysql_database"))
                }
                Engine::Mariadb => {
                    put("MARIADB_ROOT_PASSWORD", ctx.secret(d, "mariadb_root_password", &mut n.notes));
                    put("MARIADB_DATABASE", st(d, "mariadb_database").into());
                    put("MARIADB_USER", st(d, "mariadb_user").into());
                    put("MARIADB_PASSWORD", ctx.secret(d, "mariadb_password", &mut n.notes));
                    (st(d, "mariadb_user"), st(d, "mariadb_database"))
                }
                Engine::Mongo => {
                    put("MONGO_INITDB_ROOT_USERNAME", st(d, "mongo_initdb_root_username").into());
                    put("MONGO_INITDB_ROOT_PASSWORD", ctx.secret(d, "mongo_initdb_root_password", &mut n.notes));
                    put("MONGO_INITDB_DATABASE", st(d, "mongo_initdb_database").into());
                    (st(d, "mongo_initdb_root_username"), st(d, "mongo_initdb_database"))
                }
                Engine::Redis => {
                    put("REDIS_PASSWORD", ctx.secret(d, "redis_password", &mut n.notes));
                    ("default", "")
                }
            };
            n.env = env;
            n.kind = "database";
            n.spec.source = Source::Image { image: st(d, "image").into() };
            let containers = ctx.containers("coolify.databaseUuid", uuid);
            fill_from_containers(&mut n, &containers);
            if n.spec.mounts.is_empty() {
                n.spec.mounts = stored_volumes(&ctx, model, id(d, "id"));
            }
            match engine {
                // Coolify puts the password on the command line; read it from the env instead.
                Engine::Redis => n.spec.command = crate::databases::command(Engine::Redis),
                _ => n.spec.command = containers.first().and_then(|c| c.cmd.clone().filter(|cmd| Some(cmd) != c.image_cmd.as_ref())),
            }
            n.spec.ports = port_mappings(st(d, "ports_mappings"), &mut n.notes);
            if d["is_public"].as_bool() == Some(true)
                && let Some(port) = d["public_port"].as_u64()
            {
                n.spec.ports.push(Port { container: crate::databases::preset(engine).port, host: port as u16, protocol: Protocol::Tcp });
                n.notes.push(format!("its public port {port} moves from Coolify's TCP proxy to the database container"));
            }
            n.spec.health = Health { kind: HealthKind::Command, command: Some(crate::databases::health(engine).into()), start_period_s: 30, ..Health::default() };
            if engine == Engine::Mariadb {
                n.spec.health.command = Some("mariadb-admin ping -h 127.0.0.1 -uroot -p\"$MARIADB_ROOT_PASSWORD\" --silent".into());
            }
            n.spec.rollout.strategy = Strategy::StopFirst;
            n.spec.rollout.health_timeout_s = 600;
            n.spec.resources = limits(d);
            n.spec.database = Some(DatabaseSpec { engine, user: user.into(), database: database.into(), replicas: vec![] });
            n.old = Old::Labeled { label: format!("coolify.databaseUuid={uuid}") };
            let names = std::iter::once(uuid.to_string()).chain(containers.iter().flat_map(|c| std::iter::once(c.name.clone()).chain(c.aliases.iter().cloned()))).collect();
            set_names(&mut n, names, networks.get(&id(d, "destination_id")));
            n.cutover = if st(d, "status").starts_with("running") { Cutover::Swap } else { Cutover::Idle };
            plan.services.push(n);
        }
    }

    for s in ctx.table("services") {
        let Some((project, pid, team)) = envs.get(&id(s, "environment_id")).cloned() else { continue };
        let uuid = st(s, "uuid");
        let mut n = base(&project, st(s, "name"), uuid);
        n.kind = "compose";
        let (env, _) = ctx.env("App\\Models\\Service", id(s, "id"), (id(s, "environment_id"), pid, team), &mut n.notes);
        n.env = env;
        match compose_from_coolify(st(s, "docker_compose"), &n.env) {
            Ok(yaml) => n.spec.source = Source::Compose { yaml },
            Err(e) => {
                n.cutover = Cutover::Unsupported;
                n.notes.push(format!("its compose file could not be read ({e:#}); it keeps running untouched"));
                plan.services.push(n);
                continue;
            }
        }
        n.spec.compose = Some(ComposeSpec::default());
        for app in ctx.table("service_applications").iter().filter(|a| id(a, "service_id") == id(s, "id")) {
            let exposed = ctx.containers("coolify.serviceUuid", uuid).into_iter().find(|c| c.labels.get("coolify.service.subName").is_some_and(|x| x == st(app, "name"))).and_then(|c| c.exposed.first().copied());
            n.domains.extend(domains(st(app, "fqdn"), exposed.unwrap_or(80), Some(st(app, "name")), &Default::default()));
        }
        n.old = Old::Labeled { label: format!("coolify.serviceUuid={uuid}") };
        let running = ctx.containers("coolify.serviceUuid", uuid).iter().any(|c| c.running);
        n.cutover = if running { Cutover::Swap } else { Cutover::Idle };
        plan.services.push(n);
    }

    super::link_projects(&mut plan.services, |s| std::iter::once(s.key.rsplit('#').next().unwrap_or_default().to_string()).chain(s.spec.aliases.iter().cloned()).collect(), |s| {
        s.env.values().cloned().collect::<Vec<_>>().join("\n")
    });
    let fqdn = ctx.table("instance_settings").first().map(|i| st(i, "fqdn").to_string()).unwrap_or_default();
    plan.panel_domain = reqwest::Url::parse(&fqdn).ok().and_then(|u| u.host_str().map(String::from));
    let mut hosts: HashSet<String> = plan.services.iter().flat_map(|s| s.domains.iter().map(|d| d.host.clone())).collect();
    hosts.extend(plan.panel_domain.clone());
    plan.certs = acme.map(|a| super::easypanel::certs(a, &hosts)).unwrap_or_default();
    plan.notes.push("Coolify's deploy webhooks (its API tokens and Git webhook URLs) are not served by Tugport; use the service deploy hooks instead".into());
    plan
}

/// A planned service with Tugport defaults; the key carries the uuid so names never collide.
fn base(project: &str, name: &str, uuid: &str) -> NewService {
    NewService {
        key: format!("{project}/{name}#{uuid}"),
        project: project.into(),
        name: tugport_name(name),
        kind: "app",
        spec: ServiceSpec { no_new_privileges: false, ..Default::default() },
        env: BTreeMap::new(),
        files: vec![],
        domains: vec![],
        hook: None,
        image: None,
        commit: None,
        old: Old::None,
        cutover: Cutover::Parallel,
        overlays: vec![],
        notes: vec![],
    }
}

/// Old names stay reachable: as aliases on Tugport's network, and on Coolify's network
/// while old services still run.
fn set_names(n: &mut NewService, mut names: Vec<String>, network: Option<&String>) {
    names.sort();
    names.dedup();
    names.retain(|a| *a != n.name && !a.is_empty());
    n.spec.aliases = names.clone();
    if let Some(net) = network {
        n.overlays = vec![(net.clone(), names)];
    }
}

/// Image, mounts and Coolify's own variables from the container that runs it.
fn fill_from_containers(n: &mut NewService, containers: &[&Running]) {
    let Some(c) = containers.first() else { return };
    n.image = Some(c.image_id.clone()).filter(|i| !i.is_empty());
    for (kind, source, target, rw) in &c.mounts {
        match kind.as_str() {
            "volume" => n.spec.mounts.push(Mount::Volume { name: source.clone(), target: target.clone(), readonly: !rw }),
            "bind" if source == "/var/run/docker.sock" => {
                n.spec.docker_socket = true;
                n.notes.push("has access to the Docker socket".into());
            }
            "bind" => n.spec.mounts.push(Mount::Bind { source: source.clone(), target: target.clone(), readonly: !rw }),
            _ => {}
        }
    }
    for kv in &c.env {
        if let Some((k, v)) = kv.split_once('=')
            && (k.starts_with("COOLIFY_") || ["SOURCE_COMMIT", "PORT", "HOST"].contains(&k))
        {
            n.env.entry(k.into()).or_insert_with(|| v.into());
        }
    }
}

fn stored_volumes(ctx: &Ctx, model: &str, resource: i64) -> Vec<Mount> {
    ctx.table("local_persistent_volumes")
        .iter()
        .filter(|v| st(v, "resource_type") == format!("App\\Models\\{model}") && id(v, "resource_id") == resource)
        .map(|v| match st(v, "host_path") {
            "" => Mount::Volume { name: st(v, "name").into(), target: st(v, "mount_path").into(), readonly: false },
            host => Mount::Bind { source: host.into(), target: st(v, "mount_path").into(), readonly: false },
        })
        .collect()
}

/// `8080:80,5353:53/udp`
fn port_mappings(s: &str, notes: &mut Vec<String>) -> Vec<Port> {
    if s.split(',').any(|m| m.split(':').count() > 2) {
        notes.push("a published port was bound to one address; Tugport publishes it on all addresses".into());
    }
    s.split(',')
        .filter_map(|m| {
            let (m, protocol) = match m.trim().split_once('/') {
                Some((m, "udp")) => (m, Protocol::Udp),
                Some((m, _)) => (m, Protocol::Tcp),
                None => (m.trim(), Protocol::Tcp),
            };
            let (host, container) = m.rsplit_once(':')?;
            Some(Port { host: host.rsplit(':').next()?.parse().ok()?, container: container.parse().ok()?, protocol })
        })
        .collect()
}

/// `512m`, `1g`, `0.5` -> Tugport limits ("0" means none).
fn limits(v: &Value) -> crate::model::Resources {
    let mem = st(v, "limits_memory").to_lowercase();
    let mb = mem.trim_end_matches(['b', 'm', 'g', 'k']).parse::<f64>().ok().filter(|m| *m > 0.0).map(|m| match mem.trim_end_matches('b').chars().last() {
        Some('g') => (m * 1024.0) as u64,
        Some('k') => (m / 1024.0) as u64,
        Some('m') => m as u64,
        _ => (m / 1024.0 / 1024.0) as u64,
    });
    crate::model::Resources { cpus: st(v, "limits_cpus").parse().ok().filter(|c: &f64| *c > 0.0), memory_mb: mb.filter(|m| *m > 0), memory_reservation_mb: None }
}

/// Coolify's `fqdn`: comma-separated URLs; a port in the URL is the container port.
fn domains(fqdn: &str, default_port: u16, container: Option<&str>, options: &crate::model::DomainOptions) -> Vec<NewDomain> {
    fqdn.split(',')
        .filter_map(|u| reqwest::Url::parse(u.trim()).ok())
        .filter_map(|u| {
            Some(NewDomain {
                host: u.host_str()?.to_lowercase(),
                path: u.path().to_string(),
                port: u.port().unwrap_or(default_port),
                container: container.map(String::from),
                https: u.scheme() == "https",
                options: options.clone(),
            })
        })
        .collect()
}

/// Coolify's generated compose file, made Tugport's: no fixed container names (the stopped old
/// containers keep theirs) but those names as aliases, no proxy labels, no Coolify-made networks,
/// existing volumes reused, and the variables Coolify kept in `.env` inlined.
pub fn compose_from_coolify(yaml: &str, env: &BTreeMap<String, String>) -> Result<String> {
    use serde_yaml::{Mapping, Value as Y};
    let mut doc: Y = serde_yaml::from_str(yaml)?;
    let services = doc.get_mut("services").and_then(|s| s.as_mapping_mut()).ok_or_else(|| anyhow!("no services"))?;
    for (_, svc) in services.iter_mut() {
        let Some(svc) = svc.as_mapping_mut() else { continue };
        let old_name = svc.remove("container_name");
        svc.remove("env_file");
        if let Some(Y::Sequence(labels)) = svc.get_mut("labels") {
            labels.retain(|l| l.as_str().is_some_and(|l| !l.starts_with("coolify.") && !l.starts_with("traefik.") && !l.starts_with("caddy")));
        }
        let mut net = Mapping::new();
        if let Some(name) = old_name {
            net.insert("aliases".into(), Y::Sequence(vec![name]));
        }
        let mut nets = Mapping::new();
        nets.insert("default".into(), Y::Mapping(net));
        svc.insert("networks".into(), Y::Mapping(nets));
        let environment = svc.entry("environment".into()).or_insert_with(|| Y::Mapping(Mapping::new()));
        if let Y::Mapping(m) = environment {
            for (k, v) in env {
                m.entry(k.as_str().into()).or_insert_with(|| v.as_str().into());
            }
        }
    }
    if let Some(root) = doc.as_mapping_mut() {
        root.remove("networks");
        if let Some(Y::Mapping(vols)) = root.get_mut("volumes") {
            for (_, cfg) in vols.iter_mut() {
                if let Y::Mapping(m) = cfg
                    && m.contains_key("name")
                {
                    m.insert("external".into(), true.into());
                }
            }
        }
    }
    Ok(serde_yaml::to_string(&doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Made with Laravel's own Encrypter (coolify image), key "k" x 32.
    const KEY: &str = "base64:a2tra2tra2tra2tra2tra2tra2tra2tra2tra2tra2s=";
    const SERIALIZED: &str = "eyJpdiI6IkE4NmdRQ0pBRm1BcmZiNFpCc1F4M0E9PSIsInZhbHVlIjoiY1VxQmM5eVZ6Ull0cUtOdWJhUS9tMDlMb1RmRXQvaEhBd3lFVU4yUWdZdz0iLCJtYWMiOiIzYzNhNDRlM2Y1ZTgzYzZhNWRiMGU0NTQxZGU5NmNkY2U4YTgzODI2MmU4ZTU2ZDQxZDg0ZDAxYmQ2MDM1MTI1IiwidGFnIjoiIn0=";
    const STRING: &str = "eyJpdiI6IlJ2RGo5cFRPQWk3akE1dHgvRXVybXc9PSIsInZhbHVlIjoic0RmREdPckZnMUs2Nm9Ga3Q1akc2UT09IiwibWFjIjoiZWZjNmI5ZTlhYTRhNDY2ZTdmYzY3YzU4MWEwMzUwY2JhM2YzMDBiYjZiMTJhMWViOGFjNzQzMTMyMjFkOTkxMCIsInRhZyI6IiJ9";
    const SHARED_REF: &str = "eyJpdiI6IndrZ0dEU0cwT0RwTTNobVVhYm8waUE9PSIsInZhbHVlIjoielFoSURhcUNHR1NKK05WVUQ3dkRjVlJobmhsOTluSGZCY1NWakg5Uk5lUHVtc0ZQaFZSSU5oaURzS0pSTFdwWXBTaUxRbm94eVpFcXhVTkxoUjdFOGc9PSIsIm1hYyI6IjY3ODhkNzc1YTk2NzUzZDQ5Yzk3YWUyNmEyNmU2YTU1MTk2ZGMyYzQ4NjRjMzQwMDBjMDU2OTRhZjFmZjU2NjQiLCJ0YWciOiIifQ==";
    const SHARED: &str = "eyJpdiI6InpqYUhlS1YzelQ1TmpKbU04d29wUFE9PSIsInZhbHVlIjoiekJwMnNQcHBpYnJKSzRubnZuUXBFN1dUUDVhWmFJNStsOEg0MVZPRW83QT0iLCJtYWMiOiI4NGNkZmJlMmU4YTU3ZmRlNDRkYWQxYzNlNmJiYzljM2I5M2IyMmQ1ZDUxZTAyYjU3NDY0OTM3ZWQ2MzZlMGYzIiwidGFnIjoiIn0=";

    #[test]
    fn decrypts_laravel_values() {
        let key = app_key(KEY).unwrap();
        assert_eq!(decrypt(&key, SERIALIZED).unwrap(), "hello from coolify");
        assert_eq!(decrypt(&key, STRING).unwrap(), "dbpass123");
        assert_eq!(decrypt(&key, SHARED).unwrap(), "s3cr3t ü");
        assert!(decrypt(&[0u8; 32], STRING).is_err(), "a wrong key must fail the MAC");
        assert_eq!(unserialize("s:3:\"a;b\";").as_deref(), Some("a;b"));
    }

    #[test]
    fn maps_a_coolify_install() {
        let key = app_key(KEY).unwrap();
        let mut rows: HashMap<String, Vec<Value>> = HashMap::new();
        rows.insert("projects".into(), vec![json!({"id": 1, "name": "Shop", "team_id": 0}), json!({"id": 2, "name": "cache", "team_id": 0})]);
        rows.insert("environments".into(), vec![json!({"id": 1, "name": "production", "project_id": 1}), json!({"id": 2, "name": "staging", "project_id": 2})]);
        rows.insert("standalone_dockers".into(), vec![json!({"id": 0, "network": "coolify"})]);
        rows.insert("applications".into(), vec![json!({
            "id": 2, "uuid": "app1", "name": "api", "fqdn": "https://api.example.com,http://alt.example.com:8080/x", "build_pack": "nixpacks",
            "git_repository": "o/r", "git_branch": "main", "base_directory": "/", "start_command": "npm start", "ports_exposes": "3000",
            "ports_mappings": null, "environment_id": 1, "destination_id": 0, "source_type": "App\\Models\\GithubApp", "source_id": 0,
            "status": "running:healthy", "limits_memory": "512m", "limits_cpus": "0", "redirect": "www", "health_check_enabled": false
        })]);
        rows.insert("application_settings".into(), vec![json!({"application_id": 2, "custom_internal_name": null})]);
        rows.insert("standalone_postgresqls".into(), vec![json!({
            "id": 1, "uuid": "pg1", "name": "db", "postgres_user": "shop", "postgres_password": STRING, "postgres_db": "shop",
            "image": "postgres:16-alpine", "environment_id": 1, "destination_id": 0, "status": "running:healthy", "is_public": false, "limits_memory": "0", "limits_cpus": "0"
        })]);
        rows.insert("standalone_redis".into(), vec![json!({"id": 1, "uuid": "rd1", "name": "redis", "image": "redis:7", "environment_id": 2, "destination_id": 0, "status": "running:healthy", "is_public": true, "public_port": 6380, "limits_memory": "0"})]);
        rows.insert("environment_variables".into(), vec![
            json!({"key": "GREETING", "value": SERIALIZED, "is_preview": false, "is_runtime": true, "resourceable_type": "App\\Models\\Application", "resourceable_id": 2}),
            json!({"key": "GREETING", "value": STRING, "is_preview": true, "is_runtime": true, "resourceable_type": "App\\Models\\Application", "resourceable_id": 2}),
            json!({"key": "DB_URL", "value": SHARED_REF, "is_preview": false, "is_runtime": true, "resourceable_type": "App\\Models\\Application", "resourceable_id": 2}),
            json!({"key": "CACHE", "value": STRING, "is_preview": false, "is_runtime": false, "resourceable_type": "App\\Models\\Application", "resourceable_id": 2}),
            json!({"key": "REDIS_PASSWORD", "value": STRING, "is_preview": false, "is_runtime": true, "resourceable_type": "App\\Models\\StandaloneRedis", "resourceable_id": 1}),
        ]);
        rows.insert("shared_environment_variables".into(), vec![json!({"key": "DB_PASS", "type": "project", "project_id": 1, "team_id": 0, "value": SHARED})]);
        let running = vec![
            Running {
                name: "app1-20261022T101010".into(),
                labels: HashMap::from([("coolify.applicationUuid".to_string(), "app1".to_string())]),
                image_id: "sha256:aaa".into(),
                env: vec!["COOLIFY_FQDN=api.example.com".into(), "GREETING=ignored".into(), "PATH=/bin".into()],
                mounts: vec![("volume".into(), "app1-data".into(), "/data".into(), true)],
                running: true,
                ..Default::default()
            },
            Running {
                name: "pg1".into(),
                labels: HashMap::from([("coolify.databaseUuid".to_string(), "pg1".to_string())]),
                image_id: "sha256:bbb".into(),
                cmd: Some(vec!["postgres".into()]),
                image_cmd: Some(vec!["postgres".into()]),
                mounts: vec![("volume".into(), "postgres-data-pg1".into(), "/var/lib/postgresql/data".into(), true)],
                running: true,
                ..Default::default()
            },
        ];
        let plan = map(&rows, &running, &key, None, "4.4.7");
        assert_eq!(plan.projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["shop", "cache-staging"]);
        let get = |k: &str| plan.services.iter().find(|s| s.key.ends_with(k)).unwrap();

        let api = get("#app1");
        assert_eq!(api.cutover, Cutover::Parallel);
        assert_eq!(api.env["GREETING"], "hello from coolify");
        assert_eq!(api.env["DB_URL"], "postgres://u:s3cr3t ü@db:5432/x");
        assert_eq!(api.env["NIXPACKS_START_CMD"], "npm start");
        assert_eq!(api.env["COOLIFY_FQDN"], "api.example.com");
        assert!(api.notes.iter().any(|n| n.contains("build-time-only")));
        assert!(matches!(&api.spec.source, Source::Git { repo, .. } if repo == "https://github.com/o/r"));
        assert_eq!(api.image.as_deref(), Some("sha256:aaa"));
        assert_eq!(api.spec.mounts, vec![Mount::Volume { name: "app1-data".into(), target: "/data".into(), readonly: false }]);
        assert_eq!(api.spec.resources.memory_mb, Some(512));
        assert_eq!((api.domains[0].host.as_str(), api.domains[0].port, api.domains[0].https), ("api.example.com", 3000, true));
        assert_eq!((api.domains[1].port, api.domains[1].path.as_str()), (8080, "/x"));
        assert_eq!(api.domains[0].options.www_redirect, Some(WwwRedirect::ToWww));
        assert!(api.spec.aliases.contains(&"app1-20261022T101010".to_string()));

        let db = get("#pg1");
        assert_eq!((db.kind, db.cutover), ("database", Cutover::Swap));
        assert_eq!(db.env["POSTGRES_PASSWORD"], "dbpass123");
        assert_eq!(db.spec.command, None);
        assert_eq!(db.overlays, vec![("coolify".to_string(), vec!["pg1".to_string()])]);

        let redis = get("#rd1");
        assert_eq!(redis.project, "cache-staging");
        assert_eq!(redis.env["REDIS_PASSWORD"], "dbpass123");
        assert_eq!(redis.spec.ports, vec![Port { container: 6379, host: 6380, protocol: Protocol::Tcp }]);
        assert_eq!(redis.spec.mounts, vec![], "no container and no stored volumes");
    }

    #[test]
    fn rewrites_coolify_compose() {
        let yaml = "services:\n  echo:\n    image: whoami\n    environment:\n      A: '1'\n    container_name: echo-u1\n    labels: [coolify.managed=true, traefik.enable=true, keep=me]\n    networks:\n      u1: null\n    env_file: [.env]\n    volumes: ['u1_state:/state']\nvolumes:\n  u1_state:\n    name: u1_state\nnetworks:\n  u1:\n    name: u1\n    external: true\n";
        let out = compose_from_coolify(yaml, &BTreeMap::from([("B".to_string(), "2".to_string()), ("A".to_string(), "x".to_string())])).unwrap();
        let y: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let echo = &y["services"]["echo"];
        assert!(echo.get("container_name").is_none() && echo.get("env_file").is_none());
        assert_eq!(echo["networks"]["default"]["aliases"][0].as_str(), Some("echo-u1"));
        assert_eq!(echo["labels"].as_sequence().unwrap().len(), 1);
        assert_eq!((echo["environment"]["A"].as_str(), echo["environment"]["B"].as_str()), (Some("1"), Some("2")));
        assert_eq!(y["volumes"]["u1_state"]["external"].as_bool(), Some(true));
        assert!(y.get("networks").is_none());
    }
}
