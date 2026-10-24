//! Moving a server from Easypanel or Coolify into Tugport.
//!
//! 1. Import: projects, services, env, domains, certificates and deploy tokens become
//!    Tugport rows. Nothing runs yet.
//! 2. Databases and services that must never run twice move one at a time: the old copy
//!    stops, Tugport's starts on the same data and joins the old panel's networks under the
//!    old names, so the old apps keep reaching it.
//! 3. Every other app starts next to its old copy, from the image the old copy runs.
//! 4. Switch: the old proxy stops and Tugport's proxy (waiting for ports 80/443) takes over.
//!    Then the old apps and the old panel stop.
//!
//! Each step is recorded in a journal (the `migration` state key), so a rerun resumes where
//! the last one stopped and `tugport migrate rollback` undoes everything in reverse.

mod coolify;
mod easypanel;

use crate::app::{App, AppState, Settings};
use crate::model::{BasicUser, DatabaseSpec, DomainOptions, Engine, Health, HealthKind, Port, Protocol, ServiceSpec, Source, Strategy};
use crate::store::{self, Service};
use crate::{databases, deploy, git, runtime, util};
use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::models::{EndpointSettings, NetworkConnectRequest, NetworkCreateRequest, NetworkDisconnectRequest};
use bollard::query_parameters::ListContainersOptions;
use regex::Regex;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

#[derive(clap::Subcommand)]
pub enum Command {
    /// Print the panels found on this server (`easypanel 2.38.0`), one per line.
    Detect,
    /// Move this server's Easypanel projects into Tugport.
    Easypanel {
        /// Print the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Move this server's Coolify projects into Tugport.
    Coolify {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Undo the migration: start the old panel, its proxy and services again.
    Rollback {
        #[arg(long)]
        yes: bool,
    },
}

/// A panel found on this server, for the installer's "migrate?" question.
pub struct Found {
    pub panel: &'static str,
    pub version: String,
}

pub async fn detect() -> Vec<Found> {
    let Ok(docker) = Docker::connect_with_local_defaults() else { return vec![] };
    let mut out = vec![];
    if let Some(version) = easypanel::detect(&docker).await {
        out.push(Found { panel: "easypanel", version });
    }
    if let Some(version) = coolify::detect(&docker).await {
        out.push(Found { panel: "coolify", version });
    }
    out
}

pub fn main(cmd: Command) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
        match cmd {
            Command::Detect => {
                for f in detect().await {
                    println!("{} {}", f.panel, f.version);
                }
                Ok(())
            }
            Command::Easypanel { dry_run, yes } => migrate("easypanel", dry_run, yes).await,
            Command::Coolify { dry_run, yes } => migrate("coolify", dry_run, yes).await,
            Command::Rollback { yes } => rollback(&open()?, yes).await,
        }
    })
}

/// Everything one migration moves.
#[derive(Default)]
struct Plan {
    panel: &'static str,
    version: String,
    projects: Vec<NewProject>,
    services: Vec<NewService>,
    certs: Vec<Cert>,
    github_token: Option<String>,
    /// (host, user, password) for private images.
    registries: Vec<(String, String, String)>,
    acme_email: Option<String>,
    panel_domain: Option<String>,
    /// Tugport also serves its API here after the switch, so old webhook URLs keep working.
    legacy_listen: Option<String>,
    proxy: Old,
    old_panel: Old,
    /// Instance-wide things that do not move.
    notes: Vec<String>,
}

struct NewProject {
    name: String,
    vars: BTreeMap<String, String>,
}

struct NewService {
    /// The old panel's name (`project/service`); the journal key.
    key: String,
    project: String,
    name: String,
    kind: &'static str,
    spec: ServiceSpec,
    env: BTreeMap<String, String>,
    files: Vec<(String, String)>,
    domains: Vec<NewDomain>,
    /// The old deploy webhook token; its URL keeps working.
    hook: Option<String>,
    /// The image the old copy runs, deployed as-is so nothing is rebuilt during the move.
    image: Option<String>,
    commit: Option<String>,
    old: Old,
    cutover: Cutover,
    /// Old networks Tugport's copy joins while old services still run, with the names it answers to.
    overlays: Vec<(String, Vec<String>)>,
    /// What does not carry over exactly.
    notes: Vec<String>,
    /// A token that was inside the old Git URL: (provider base URL, token), stored encrypted.
    git_secret: Option<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cutover {
    /// Old and new must never run together (databases, stop-first services, published ports):
    /// stop the old copy, then start Tugport's.
    Swap,
    /// Start Tugport's copy next to the old one; the old one stops after the proxy switch.
    Parallel,
    /// Imported but not started (it is stopped in the old panel too).
    Idle,
    /// Not imported; it keeps running untouched.
    Unsupported,
}

struct NewDomain {
    host: String,
    path: String,
    port: u16,
    container: Option<String>,
    https: bool,
    options: DomainOptions,
}

struct Cert {
    host: String,
    cert_pem: String,
    key_pem: String,
}

/// How to stop and start the old copy of something.
#[derive(Default, PartialEq)]
enum Old {
    #[default]
    None,
    /// A swarm service, stopped by scaling it to zero.
    Swarm { service: String },
    /// Plain containers carrying this label (`key=value`).
    Labeled { label: String },
    /// One container by name.
    Container { name: String },
}

/// Old panels keep Git tokens inside repository URLs (`https://user:token@host/o/r.git`).
/// Tugport keeps them as encrypted providers instead, so no token sits in a service's
/// settings or in a printed plan: the URL loses its credentials and points at a provider.
fn lift_git_credentials(plan: &mut Plan) {
    for s in &mut plan.services {
        let Source::Git { provider, repo, .. } = &mut s.spec.source else { continue };
        let Some((base, path, secret)) = credentialed_git(repo) else { continue };
        if provider.is_none() {
            *repo = path;
            *provider = Some(format!("$git:{base}"));
            s.git_secret = Some((base, secret));
        } else {
            *repo = redact_userinfo(repo);
        }
    }
}

/// `https://user:secret@host/owner/name.git` -> (`https://host`, `owner/name`, secret).
fn credentialed_git(url: &str) -> Option<(String, String, String)> {
    let u = reqwest::Url::parse(url).ok()?;
    let secret = u.password().filter(|p| !p.is_empty()).map(String::from).or_else(|| (!u.username().is_empty()).then(|| u.username().to_string()))?;
    let host = u.host_str()?;
    let base = format!("{}://{host}{}", u.scheme(), u.port().map(|p| format!(":{p}")).unwrap_or_default());
    let path = u.path().trim_matches('/').trim_end_matches(".git").to_string();
    Some((base, path, secret))
}

fn redact_userinfo(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        _ => url.to_string(),
    }
}

/// Placeholder provider id in planned specs; import swaps in the GitHub provider it creates.
const GITHUB: &str = "$github";

/// Tugport names are `[a-z][a-z0-9-]*`: `cas_ban_bot` becomes `cas-ban-bot`.
fn tugport_name(old: &str) -> String {
    let mut n = String::new();
    for c in old.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if c != '-' || !(n.is_empty() || n.ends_with('-')) {
            n.push(c);
        }
    }
    if !n.starts_with(|c: char| c.is_ascii_lowercase()) {
        n.insert_str(0, "s-");
    }
    n.truncate(40);
    n.trim_end_matches('-').to_string()
}

/// Services that mention another project's service by one of its old names join that
/// project's network, so cross-project references keep resolving.
fn link_projects(services: &mut [NewService], names: impl Fn(&NewService) -> Vec<String>, text: impl Fn(&NewService) -> String) {
    let mut joins: Vec<(usize, String)> = vec![];
    for (i, target) in services.iter().enumerate() {
        let qualified = names(target);
        for s in services.iter().filter(|s| s.project != target.project && s.cutover != Cutover::Unsupported) {
            let t = text(s);
            if qualified.iter().any(|n| mentions(&t, n)) {
                joins.push((i, runtime::network_name(&s.project)));
            }
        }
    }
    for (i, net) in joins {
        if !services[i].spec.networks.contains(&net) {
            services[i].spec.networks.push(net);
        }
    }
    // Two services answering to one name on one network would make it resolve at random.
    let mut owners: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, s) in services.iter().enumerate().filter(|(_, s)| s.cutover != Cutover::Unsupported) {
        for net in std::iter::once(runtime::network_name(&s.project)).chain(s.spec.networks.iter().cloned()) {
            for n in std::iter::once(&s.name).chain(&s.spec.aliases) {
                owners.entry((net.clone(), n.clone())).or_default().push(i);
            }
        }
    }
    for ((net, n), list) in owners.into_iter().filter(|(_, v)| v.len() > 1) {
        for i in list {
            services[i].notes.push(format!("another service also answers to {n} on {net}"));
        }
    }
}

/// `host` appears in `text` as a whole host name (not as part of a longer one).
fn mentions(text: &str, host: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    text.match_indices(host).any(|(i, m)| {
        let before = text.get(..i).and_then(|t| t.chars().next_back());
        let after = text.get(i + m.len()..).and_then(|t| t.chars().next());
        !before.is_some_and(|c| part(c) || c == '.') && !after.is_some_and(part)
    })
}

impl NewService {
    /// A planned service with Tugport's defaults. Imported services keep
    /// `no_new_privileges: false`, so they behave exactly as before.
    fn new(key: String, project: String, name: &str) -> Self {
        NewService {
            key,
            project,
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
            git_secret: None,
        }
    }

    /// Old and new must never run together: a running service swaps instead.
    fn exclusive(&mut self) {
        if self.cutover == Cutover::Parallel {
            self.cutover = Cutover::Swap;
        }
    }

    /// What both panels' databases share: the engine's health check, a stop-first rollout
    /// with time to recover, and the public port on the database container.
    fn database(&mut self, engine: Engine, user: &str, database: &str, public: Option<u16>) {
        self.kind = "database";
        let check = match engine {
            // The official healthcheck.sh needs a user the old panels never created.
            Engine::Mariadb => "mariadb-admin ping -h 127.0.0.1 -uroot -p\"$MARIADB_ROOT_PASSWORD\" --silent",
            e => databases::preset(e).health,
        };
        self.spec.health = Health { kind: HealthKind::Command, command: Some(check.into()), start_period_s: 30, ..Health::default() };
        self.spec.rollout.strategy = Strategy::StopFirst;
        self.spec.rollout.health_timeout_s = 600;
        self.spec.database = Some(DatabaseSpec { engine, user: user.into(), database: database.into(), primary: None });
        if let Some(port) = public {
            self.spec.ports.push(Port { container: databases::preset(engine).port, host: port, protocol: Protocol::Tcp });
        }
        self.exclusive();
    }
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

/// A trimmed string field of foreign JSON; missing or not a string reads as "".
fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("").trim()
}

fn nonempty(s: &str) -> Option<&str> {
    Some(s).filter(|s| !s.is_empty())
}

/// A port number; out-of-range numbers are `None` rather than wrapped onto another port (80, say).
fn port(v: &Value) -> Option<u16> {
    v.as_u64().and_then(|p| u16::try_from(p).ok())
}

/// Set `k` to `v` unless it is set already or `v` is empty.
fn default_env(env: &mut BTreeMap<String, String>, k: &str, v: &str) {
    if !v.is_empty() {
        env.entry(k.into()).or_insert_with(|| v.into());
    }
}

/// A basic auth user with its password hashed; a note when it cannot be.
fn basic_user(user: &str, password: &str, notes: &mut Vec<String>) -> Option<BasicUser> {
    match crate::auth::hash_password(password) {
        Ok(hash) => Some(BasicUser { user: user.into(), hash, password: None }),
        Err(e) => {
            notes.push(format!("basic auth user {user} was not moved: {e:#}"));
            None
        }
    }
}

/// Import the certificates Traefik obtained (`acme.json`) for the hosts that move; returns those hosts.
fn take_certs(plan: &mut Plan, acme: Option<&str>) -> HashSet<String> {
    use base64::Engine as _;
    let mut hosts: HashSet<String> = plan.services.iter().flat_map(|s| s.domains.iter().map(|d| d.host.clone())).collect();
    hosts.extend(plan.panel_domain.clone());
    let Some(v) = acme.and_then(|a| serde_json::from_str::<Value>(a).ok()) else { return hosts };
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).ok().and_then(|b| String::from_utf8(b).ok());
    let mut best: BTreeMap<String, (Option<i64>, Cert)> = BTreeMap::new();
    for resolver in v.as_object().into_iter().flat_map(|o| o.values()) {
        for c in resolver["Certificates"].as_array().into_iter().flatten() {
            let (Some(cert), Some(key)) = (b64(st(c, "certificate")), b64(st(c, "key"))) else { continue };
            let not_after = crate::acme::first_cert_not_after(&cert);
            let names = std::iter::once(st(&c["domain"], "main").to_string()).chain(strings(&c["domain"]["sans"]));
            for host in names.map(|h| h.to_lowercase()).filter(|h| hosts.contains(h)) {
                if best.get(&host).is_none_or(|(have, _)| not_after > *have) {
                    best.insert(host.clone(), (not_after, Cert { host, cert_pem: cert.clone(), key_pem: key.clone() }));
                }
            }
        }
    }
    plan.certs = best.into_values().map(|(_, c)| c).collect();
    hosts
}

/// A panel's version from the image it runs: `easypanel/easypanel:2.38.0@sha256:...` -> `2.38.0`.
fn image_tag(image: &str) -> String {
    runtime::split_image(image.split('@').next().unwrap_or(image)).1.to_string()
}

/// A Git source without a provider; pushes reach it through the deploy webhook.
fn git_source(repo: &str, branch: &str, path: &str) -> Source {
    Source::Git { provider: None, repo: repo.into(), branch: branch.into(), path: path.trim_matches('/').into(), watch_paths: vec![], auto_deploy: false }
}

/// The strings in a JSON array; anything else reads as empty.
fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// `dotenv.parse` as Easypanel runs it (dotenv 16), also used for Coolify's `.env`: `KEY=value`
/// lines, quotes, `export`, comments; `\n` in double quotes becomes a newline.
fn dotenv(src: &str) -> BTreeMap<String, String> {
    static LINE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r#"(?m)^\s*(?:export\s+)?([A-Za-z0-9_.-]+)(?:\s*=\s*?|:\s+?)(\s*'(?:\\'|[^'])*'|\s*"(?:\\"|[^"])*"|\s*`(?:\\`|[^`])*`|[^#\r\n]+)?\s*(?:#.*)?$"#).ok()
    });
    let mut out = BTreeMap::new();
    let Some(re) = LINE.as_ref() else { return out };
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    for c in re.captures_iter(&src) {
        let (Some(key), raw) = (c.get(1), c.get(2).map(|m| m.as_str().trim()).unwrap_or("")) else { continue };
        let quote = raw.chars().next().filter(|q| matches!(q, '\'' | '"' | '`'));
        let mut v = quote.and_then(|q| raw.strip_prefix(q)?.strip_suffix(q)).unwrap_or(raw).to_string();
        if quote == Some('"') {
            v = v.replace("\\n", "\n").replace("\\r", "\r");
        }
        out.insert(key.as_str().to_string(), v);
    }
    out
}

/// What a migration did, so reruns resume and rollback can undo it.
#[derive(Serialize, Deserialize, Default)]
struct Journal {
    panel: String,
    projects: Vec<String>,
    services: Vec<Moved>,
    certs: Vec<String>,
    provider: Option<String>,
    /// Providers made from tokens found in Git URLs, by base URL.
    #[serde(default)]
    git_providers: BTreeMap<String, String>,
    registries: Vec<String>,
    settings_before: Option<Settings>,
    imported_at: i64,
    proxy: Option<Stopped>,
    old_panel: Option<Stopped>,
    done: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Moved {
    key: String,
    id: String,
    /// The service row, env, files, domains and hook are all in place.
    imported: bool,
    deployed: bool,
    swap: bool,
    overlays: Vec<(String, Vec<String>)>,
    /// The old copy as it was before we stopped it.
    stopped: Option<Stopped>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Stopped {
    Swarm { service: String, replicas: u64 },
    Containers {
        ids: Vec<String>,
        /// Old networks removed so Tugport's copy could take the subnets they held; created
        /// again before the containers start.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        networks: Vec<NetworkCreateRequest>,
    },
}

const JOURNAL: &str = "migration";

fn load(app: &App) -> Result<Option<Journal>> {
    app.db.get_state(JOURNAL)?.map(|s| serde_json::from_str(&s).context("the migration journal is unreadable")).transpose()
}

fn save(app: &App, j: &Journal) -> Result<()> {
    app.db.set_state(JOURNAL, &serde_json::to_string(j)?)
}

/// The local Tugport, opened the way `serve()` opens it (the server may be running too).
fn open() -> Result<AppState> {
    let paths = crate::paths::Paths::from_env();
    let master = crate::crypto::load_master(&paths.etc).context("Tugport is not set up on this server yet; install and start it first")?;
    let db = crate::db::Db::open(&paths.db())?;
    let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;
    if !paths.init_binary().exists() {
        bail!("start Tugport once first (it installs the container init at {})", paths.init_binary().display());
    }
    let selflog = crate::logs::SelfLog::new(paths.logs().join("_tugport"));
    let app = App::new(paths, db, crate::crypto::Keyring::new(&master), docker, selflog, 2)?;
    crate::nodes::load(&app)?;
    Ok(app)
}

async fn migrate(panel: &'static str, dry_run: bool, yes: bool) -> Result<()> {
    let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;
    let mut plan = match panel {
        "easypanel" => easypanel::plan(&docker).await?,
        _ => coolify::plan(&docker).await?,
    };
    lift_git_credentials(&mut plan);
    review_host_access(&mut plan);
    print_plan(&plan);
    if dry_run {
        return Ok(());
    }
    let app = open()?;
    let mut j = load(&app)?.unwrap_or_default();
    if !j.panel.is_empty() && j.panel != panel {
        bail!("a {} migration is recorded on this server; roll it back first", j.panel);
    }
    if j.done {
        println!("\nThis server was already migrated. `tugport migrate rollback` undoes it.");
        return Ok(());
    }
    if !yes && !confirm("\nMove these services to Tugport now?")? {
        bail!("nothing was changed");
    }
    j.panel = panel.into();
    save(&app, &j)?;
    import(&app, &plan, &mut j).await?;
    cutover(&app, &plan, &mut j).await?;
    println!("\nDone. The old panel is stopped, not removed: `tugport migrate rollback` brings it back.");
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("pass --yes to run without a terminal");
    }
    Ok(matches!(util::prompt(&format!("{question} [y/N]"))?.as_str(), "y" | "yes"))
}

/// The old panel may have given services host access that Tugport otherwise leaves to admins
/// with the destroy grant. Say which services keep it, turn on privileged mode for Compose
/// stacks that need it (Tugport checks their file at every deploy), and leave out services that
/// would mount Tugport's own files.
fn review_host_access(plan: &mut Plan) {
    let own = crate::paths::Paths::from_env();
    // Networks of the migrated projects (services that use another project's names join it).
    let migrated: Vec<String> = plan.projects.iter().flat_map(|p| [runtime::network_name(&p.name), runtime::overlay_name(&p.name)]).collect();
    for s in plan.services.iter_mut().filter(|s| s.cutover != Cutover::Unsupported) {
        let binds: Vec<String> = s.spec.mounts.iter().filter_map(|m| if let crate::model::Mount::Bind { source, .. } = m { Some(source.clone()) } else { None }).collect();
        let tugports = |b: &String| {
            let p = std::path::Path::new(b);
            p.components().any(|c| c == std::path::Component::ParentDir) || [&own.etc, &own.data, &own.run].iter().any(|d| p.starts_with(d))
        };
        if binds.iter().any(tugports) {
            s.cutover = Cutover::Unsupported;
            s.notes.push("it mounts Tugport's own files".into());
            continue;
        }
        // Names in Tugport's own namespace would attach another project's data or network.
        let ours = |n: &str| n.starts_with("tp-") || n.starts_with("tpm-") || n.starts_with("tugport");
        let volumes = s.spec.mounts.iter().filter_map(|m| if let crate::model::Mount::Volume { name, .. } = m { Some(name.as_str()) } else { None });
        if volumes.chain(s.spec.networks.iter().map(String::as_str).filter(|n| !migrated.iter().any(|m| m == n))).any(ours) {
            s.cutover = Cutover::Unsupported;
            s.notes.push("it uses a volume or network name that belongs to Tugport (tp-, tpm-, tugport)".into());
            continue;
        }
        let mut access: Vec<String> = binds.iter().map(|b| format!("bind mount {b}")).collect();
        for (on, what) in [(s.spec.docker_socket, "the Docker socket"), (s.spec.privileged, "privileged mode"), (s.spec.host_network, "host networking")] {
            if on {
                access.push(what.into());
            }
        }
        if !s.spec.cap_add.is_empty() {
            access.push(format!("capabilities {}", s.spec.cap_add.join(" ")));
        }
        if s.kind == "compose" && !s.spec.privileged {
            let needs = match &s.spec.source {
                Source::Compose { yaml } => crate::compose::check_untrusted_text(yaml).err().map(|e| format!("{e:#}")),
                _ => Some("its file is read from the repository at each deploy".into()),
            };
            if let Some(why) = needs {
                s.spec.privileged = true;
                access.push(format!("privileged mode for its Compose file ({why})"));
            }
        }
        if !access.is_empty() {
            s.notes.push(format!("keeps the host access it had: {}", access.join(", ")));
        }
    }
}

fn print_plan(plan: &Plan) {
    let moved: Vec<&NewService> = plan.services.iter().filter(|s| s.cutover != Cutover::Unsupported).collect();
    println!("{} {} -> Tugport: {} projects, {} services\n", plan.panel, plan.version, plan.projects.len(), moved.len());
    for p in &plan.projects {
        println!("{}{}", p.name, if p.vars.is_empty() { String::new() } else { format!("  ({} shared variables)", p.vars.len()) });
        for s in moved.iter().filter(|s| s.project == p.name) {
            let how = match s.cutover {
                Cutover::Swap => "stop old, then start",
                Cutover::Parallel => "next to the old copy",
                _ => "imported, not started",
            };
            let what = match &s.spec.source {
                Source::Git { repo, branch, .. } => format!("{repo}@{branch}"),
                Source::Compose { .. } => "compose".into(),
                Source::Image { image } => image.clone(),
                Source::Dockerfile { .. } => "dockerfile".into(),
            };
            let mut extra = vec![];
            if !s.domains.is_empty() {
                extra.push(s.domains.iter().map(|d| d.host.as_str()).collect::<Vec<_>>().join(" "));
            }
            if !s.env.is_empty() {
                extra.push(format!("{} env", s.env.len()));
            }
            let old_name = s.key.rsplit('/').next().unwrap_or_default().split('#').next().unwrap_or_default();
            if old_name != s.name {
                extra.push(format!("was {old_name}"));
            }
            if !s.spec.networks.is_empty() {
                extra.push(format!("also on {}", s.spec.networks.join(" ")));
            }
            println!("  {:<22} {:<9} {:<34} {:<22} {}", s.name, kind_label(s), clip(&what, 34), how, extra.join(", "));
        }
    }
    print_list("Needs attention", moved.iter().flat_map(|s| s.notes.iter().map(move |n| format!("{}: {n}", s.key))).collect());
    let skipped = plan.services.iter().filter(|s| s.cutover == Cutover::Unsupported).map(|s| format!("{}: {}", s.key, s.notes.join("; ")));
    print_list("Not moved", skipped.chain(plan.notes.iter().cloned()).collect());
    let hooks = moved.iter().filter(|s| s.hook.is_some()).count();
    println!("\nCertificates: {} imported, so nothing is re-issued", plan.certs.len());
    if hooks > 0 {
        println!("Deploy webhooks: {hooks} old URLs keep working{}", plan.legacy_listen.as_ref().map(|l| format!(" (Tugport answers on {l} after the switch)")).unwrap_or_default());
    }
    let mut tokens: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (base, token) in plan.services.iter().filter_map(|s| s.git_secret.as_ref()) {
        tokens.entry(base).or_default().push(token);
    }
    for (base, mut list) in tokens {
        let n = list.len();
        list.sort();
        list.dedup();
        println!("Git: {n} repositories on {base} had a token in their URL; it becomes one encrypted Git provider");
        if list.len() > 1 {
            // One provider per host means one token for all of them.
            println!("  ! they used {} different tokens and only the first one is kept, so it reads every one of those repositories", list.len());
        }
    }
    let count = |f: fn(&NewService) -> bool| moved.iter().filter(|s| f(s)).count();
    println!(
        "Cutover: databases move first ({}), then apps start next to their old copies ({}), then services that must not run twice move ({}). \
         Each moved database and service is down for a few seconds; everything else only while the proxy switches (about a second).",
        count(|s| s.cutover == Cutover::Swap && s.kind == "database"),
        count(|s| s.cutover == Cutover::Parallel),
        count(|s| s.cutover == Cutover::Swap && s.kind != "database"),
    );
}

fn print_list(title: &str, items: Vec<String>) {
    if !items.is_empty() {
        println!("\n{title}:");
        for i in items {
            println!("  - {i}");
        }
    }
}

fn kind_label(s: &NewService) -> String {
    match &s.spec.database {
        Some(d) => crate::databases::preset(d.engine).repo.to_string(),
        None => s.kind.to_string(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

async fn import(app: &AppState, plan: &Plan, j: &mut Journal) -> Result<()> {
    if j.settings_before.is_none() {
        j.settings_before = Some(app.settings());
        save(app, j)?;
    }
    let mut settings = app.settings();
    settings.acme_email = settings.acme_email.or(plan.acme_email.clone());
    settings.panel_domain = settings.panel_domain.or(plan.panel_domain.clone());
    app.db.set_setting("instance", &settings)?;
    import_credentials(app, plan, j)?;
    for p in &plan.projects {
        let project = match store::project(app, &p.name)? {
            Some(p) => p,
            None => {
                let id = util::id();
                app.db.exec("INSERT INTO projects (id, name, created_at) VALUES (?1, ?2, ?3)", params![id, p.name, util::now()])?;
                j.projects.push(id.clone());
                save(app, j)?;
                store::project(app, &id)?.ok_or_else(|| anyhow!("project {} vanished", p.name))?
            }
        };
        let existing = store::project_vars(app, &project.id)?;
        for (k, v) in p.vars.iter().filter(|(k, _)| !existing.contains_key(*k)) {
            store::set_project_var(app, &project.id, k, Some(v))?;
        }
    }
    let provider = j.provider.clone();
    for s in plan.services.iter().filter(|s| s.cutover != Cutover::Unsupported) {
        import_service(app, s, provider.as_deref(), j)?;
    }
    for c in &plan.certs {
        import_cert(app, c, j)?;
    }
    j.imported_at = util::now();
    save(app, j)?;
    println!("\nImported {} services.", j.services.len());
    Ok(())
}

/// Git providers and registry logins, each recorded as soon as it exists.
fn import_credentials(app: &App, plan: &Plan, j: &mut Journal) -> Result<()> {
    let provider = |kind, name: &str, base: &str, token: &str| {
        git::save_provider(app, None, kind, &format!("{name} (from {})", plan.panel), base, &serde_json::json!({ "token": token, "webhook_secret": util::token(40) }))
    };
    if let (Some(token), None) = (&plan.github_token, &j.provider) {
        j.provider = Some(provider(git::Kind::GithubToken, "GitHub", "https://github.com", token)?);
        save(app, j)?;
    }
    for (base, secret) in plan.services.iter().filter_map(|s| s.git_secret.as_ref()) {
        if j.git_providers.contains_key(base) {
            continue;
        }
        let kind = if base == "https://github.com" { git::Kind::GithubToken } else { git::Kind::Forgejo };
        let id = provider(kind, base.trim_start_matches("https://").trim_start_matches("http://"), base, secret)?;
        j.git_providers.insert(base.clone(), id);
        save(app, j)?;
    }
    for (host, user, password) in &plan.registries {
        if app.db.row("SELECT 1 FROM registries WHERE host = ?1", [host], |_| Ok(()))?.is_none() {
            app.db.exec(
                "INSERT INTO registries (id, host, username, password, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![util::id(), host, user, app.keys.seal_str(&format!("registry:{host}"), password)?, util::now()],
            )?;
            j.registries.push(host.clone());
            save(app, j)?;
        }
    }
    Ok(())
}

fn import_service(app: &App, s: &NewService, provider: Option<&str>, j: &mut Journal) -> Result<()> {
    let existing = store::service(app, &format!("{}.{}", s.project, s.name))?;
    let pos = j.services.iter().position(|m| m.key == s.key);
    match (existing, pos.and_then(|i| j.services.get(i))) {
        (Some(_), Some(m)) if m.imported => return Ok(()),
        // Half imported by an interrupted run: start over.
        (Some(e), Some(_)) => {
            app.db.exec("DELETE FROM services WHERE id = ?1", [&e.id])?;
        }
        (Some(_), None) => bail!("Tugport already has a service {}.{}; rename or delete it and run the migration again", s.project, s.name),
        (None, _) => {}
    }
    let project = store::project(app, &s.project)?.ok_or_else(|| anyhow!("project {} is missing", s.project))?;
    let mut spec = s.spec.clone();
    if let Source::Git { provider: p, .. } = &mut spec.source {
        if p.as_deref() == Some(GITHUB) {
            *p = provider.map(String::from);
        } else if let Some(base) = p.as_deref().and_then(|v| v.strip_prefix("$git:")) {
            *p = Some(j.git_providers.get(base).cloned().ok_or_else(|| anyhow!("no provider was made for {base}"))?);
        }
    }
    let id = util::id();
    let now = util::now();
    let moved = Moved { key: s.key.clone(), id: id.clone(), imported: false, deployed: false, swap: s.cutover == Cutover::Swap, overlays: s.overlays.clone(), stopped: None };
    match pos {
        Some(i) => j.services[i] = moved,
        None => j.services.push(moved),
    }
    save(app, j)?;
    app.db.exec(
        "INSERT INTO services (id, project_id, name, kind, spec, deploy_token_hash, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![id, project.id, s.name, s.kind, serde_json::to_string(&spec)?, s.hook.as_ref().map(util::sha256_hex), now],
    )?;
    for (k, v) in &s.env {
        store::set_env(app, &id, k, Some(v))?;
    }
    for (target, content) in &s.files {
        store::set_file(app, &id, target, Some(content))?;
    }
    for d in &s.domains {
        let added = app.db.exec(
            "INSERT INTO domains (id, service_id, host, path, port, container, https, via_tunnel, options, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9)
             ON CONFLICT(host, path) DO NOTHING",
            params![util::id(), id, d.host, d.path, d.port, d.container, d.https, serde_json::to_string(&d.options)?, now],
        )?;
        if added == 0 {
            println!("  ! {}: Tugport already routes {}{} elsewhere; that domain was not moved", s.key, d.host, d.path);
        }
    }
    if let Some(m) = j.services.iter_mut().find(|m| m.key == s.key) {
        m.imported = true;
    }
    save(app, j)
}

fn import_cert(app: &App, c: &Cert, j: &mut Journal) -> Result<()> {
    if let Err(e) = crate::proxy::certified_key(&c.cert_pem, &c.key_pem) {
        println!("  ! the certificate for {} is unusable ({e:#}); Tugport will request a new one", c.host);
        return Ok(());
    }
    let not_after = crate::acme::first_cert_not_after(&c.cert_pem);
    let current: Option<Option<i64>> = app.db.row("SELECT not_after FROM certificates WHERE host = ?1 AND cert_pem IS NOT NULL", [&c.host], |r| r.get(0))?;
    if current.flatten().is_some_and(|have| not_after.is_none_or(|n| have >= n)) {
        return Ok(());
    }
    app.db.exec(
        "INSERT INTO certificates (host, cert_pem, key_pem, not_after, source, updated_at) VALUES (?1, ?2, ?3, ?4, 'imported', ?5)
         ON CONFLICT(host) DO UPDATE SET cert_pem = excluded.cert_pem, key_pem = excluded.key_pem, not_after = excluded.not_after, source = 'imported', last_error = NULL, next_attempt_at = NULL, updated_at = excluded.updated_at",
        params![c.host, c.cert_pem, app.keys.seal_str(&crate::acme::key_context(&c.host), &c.key_pem)?, not_after, util::now()],
    )?;
    if !j.certs.contains(&c.host) {
        j.certs.push(c.host.clone());
    }
    Ok(())
}

async fn cutover(app: &AppState, plan: &Plan, j: &mut Journal) -> Result<()> {
    if plan.proxy != Old::None && j.proxy.is_none() {
        server_running()?;
    }
    let moved: Vec<&NewService> = plan.services.iter().filter(|s| j.services.iter().any(|m| m.key == s.key && m.imported)).collect();
    let projects: BTreeSet<&str> = moved.iter().map(|s| s.project.as_str()).collect();
    for p in projects {
        runtime::ensure_network(app, p).await?;
    }
    let (databases, rest): (Vec<&NewService>, Vec<&NewService>) = moved.iter().partition(|s| s.kind == "database");
    for s in databases.iter().filter(|s| s.cutover == Cutover::Swap) {
        swap(app, s, j).await?;
    }
    let parallel: Vec<&NewService> = rest.iter().copied().filter(|s| s.cutover == Cutover::Parallel).collect();
    for s in &parallel {
        let m = moved_mut(j, &s.key)?;
        if !m.deployed {
            println!("{}: starting next to the old copy", s.key);
            let svc = store::service(app, &m.id)?.ok_or_else(|| anyhow!("{} vanished", s.key))?;
            start(app, &svc, s).await.with_context(|| format!("starting {}; the old copy still serves it", s.key))?;
            moved_mut(j, &s.key)?.deployed = true;
            save(app, j)?;
        }
    }
    // The old proxy may not reach the next ones' new copies (Coolify routes by container
    // labels), so let Tugport load the imported routes first and keep their gap to the switch short.
    if j.proxy.is_none() {
        wait_for_reload(j).await;
    }
    // Compose stacks last: Tugport's copy does not take the old names, so keep their gap short.
    let mut swaps: Vec<&&NewService> = rest.iter().filter(|s| s.cutover == Cutover::Swap).collect();
    swaps.sort_by_key(|s| s.kind == "compose");
    for s in swaps {
        swap(app, s, j).await?;
    }

    switch(app, plan, j).await?;

    for s in &parallel {
        remember(app, j, |j| Ok(&mut moved_mut(j, &s.key)?.stopped), &s.old).await?;
        halt(app, &s.old, false).await?;
    }
    if plan.old_panel != Old::None {
        remember(app, j, |j| Ok(&mut j.old_panel), &plan.old_panel).await?;
        println!("Stopping the old panel");
        halt(app, &plan.old_panel, false).await?;
    }
    if let Some(listen) = &plan.legacy_listen {
        serve_legacy(app, listen).await?;
    }
    // Every old consumer is gone: drop the old networks so removing the old panel later breaks nothing.
    for m in j.services.iter().filter(|m| m.swap) {
        leave_overlays(app, &m.id, &m.overlays).await;
    }
    // Hosts without an imported certificate failed their challenges while the old proxy held
    // port 80; retry them now instead of after the backoff.
    app.db.exec("DELETE FROM state WHERE key LIKE 'acme_attempts:%'", [])?;
    app.db.exec("UPDATE certificates SET next_attempt_at = NULL WHERE cert_pem IS NULL", [])?;
    j.done = true;
    save(app, j)
}

/// Serve the API on the old panel's address too, so its deploy webhook URLs keep working.
async fn serve_legacy(app: &App, listen: &str) -> Result<()> {
    let mut settings = app.settings();
    settings.legacy_listen = Some(listen.into());
    app.db.set_setting("instance", &settings)?;
    let port = listen.rsplit(':').next().unwrap_or("3000");
    if answers(&format!("http://127.0.0.1:{port}/api/deploy/tugport-migration-probe"), None, Duration::from_secs(30)).await {
        println!("Old deploy webhook URLs now reach Tugport on {listen}");
    } else {
        println!("  ! Tugport does not answer on {listen} yet; restart it to serve the old webhook URLs");
    }
    Ok(())
}

fn moved_mut<'a>(j: &'a mut Journal, key: &str) -> Result<&'a mut Moved> {
    j.services.iter_mut().find(|m| m.key == key).ok_or_else(|| anyhow!("{key} is not in the journal"))
}

/// Record in the journal slot how an old copy runs, unless a record is there already.
async fn remember<F>(app: &App, j: &mut Journal, slot: F, old: &Old) -> Result<()>
where
    F: for<'a> Fn(&'a mut Journal) -> Result<&'a mut Option<Stopped>>,
{
    if slot(j)?.is_none() {
        let stopped = record(&app.docker, old).await?;
        *slot(j)? = Some(stopped);
        save(app, j)?;
    }
    Ok(())
}

/// Start the old copy recorded in the journal slot again and clear the slot.
async fn restore<F>(app: &App, j: &mut Journal, slot: F, say: Option<&str>) -> Result<()>
where
    F: for<'a> Fn(&'a mut Journal) -> Result<&'a mut Option<Stopped>>,
{
    if let Some(stopped) = slot(j)?.take() {
        if let Some(say) = say {
            println!("{say}");
        }
        start_old(&app.docker, &stopped).await;
    }
    save(app, j)
}

/// Stop the old copy, then start Tugport's on the same data; on failure the old one comes back.
async fn swap(app: &AppState, s: &NewService, j: &mut Journal) -> Result<()> {
    let m = moved_mut(j, &s.key)?;
    let (id, deployed) = (m.id.clone(), m.deployed);
    if !deployed {
        // Record what to restore before stopping anything; stop again on reruns, since an
        // interrupted run may have recorded without stopping.
        remember(app, j, |j| Ok(&mut moved_mut(j, &s.key)?.stopped), &s.old).await?;
        // Whatever did stop comes back, so the service is not left down until a rerun.
        println!("{}: stopping the old copy", s.key);
        if let Err(e) = halt(app, &s.old, false).await {
            println!("{}: could not stop the old copy, starting it again", s.key);
            restore(app, j, |j| Ok(&mut moved_mut(j, &s.key)?.stopped), None).await?;
            return Err(e.context(format!("stopping the old copy of {}", s.key)));
        }
        if let Source::Compose { yaml } = &s.spec.source {
            release_subnets(app, j, &s.key, &pinned_subnets(yaml)).await?;
        }
        println!("{}: starting on Tugport", s.key);
        let svc = store::service(app, &id)?.ok_or_else(|| anyhow!("{} vanished", s.key))?;
        if let Err(e) = start(app, &svc, s).await {
            println!("{}: failed, starting the old copy again", s.key);
            let _ = runtime::remove_service(app, &svc, false).await;
            restore(app, j, |j| Ok(&mut moved_mut(j, &s.key)?.stopped), None).await?;
            return Err(e.context(format!("moving {}", s.key)));
        }
        moved_mut(j, &s.key)?.deployed = true;
        save(app, j)?;
    }
    join_overlays(app, &id, &s.overlays).await;
    Ok(())
}

/// Deploy Tugport's copy and wait for it to go live.
async fn start(app: &AppState, svc: &Service, s: &NewService) -> Result<()> {
    // By image id: exactly what runs now, and Tugport's image cleanup leaves the old panel's
    // images alone (a rollback needs them).
    let image = match &s.image {
        Some(i) if svc.kind != "compose" => app.docker.inspect_image(i).await.ok().and_then(|img| img.id),
        _ => None,
    };
    let req = deploy::Request { trigger: "migration".into(), actor: Some("migration".into()), image, commit_sha: s.commit.clone(), ..Default::default() };
    let id = deploy::enqueue(app, svc, req)?;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let d = deploy::get(app, &id)?.ok_or_else(|| anyhow!("deploy {id} vanished"))?;
        if d.status == "live" {
            return Ok(());
        }
        if deploy::FINAL.contains(&d.status.as_str()) {
            let log = deploy::read_log(app, &id);
            for (_, line) in log.iter().skip(log.len().saturating_sub(12)) {
                println!("    | {line}");
            }
            bail!("{}", d.error.unwrap_or(d.status));
        }
    }
}

/// Answer to the old names on the old networks, so old apps and the old proxy reach Tugport's
/// copy. `{compose}` in an alias stands for the container's compose service.
async fn join_overlays(app: &App, service_id: &str, overlays: &[(String, Vec<String>)]) {
    for r in runtime::replicas(app, service_id).await.unwrap_or_default() {
        for (net, aliases) in overlays {
            let aliases = aliases.iter().map(|a| a.replace("{compose}", r.compose_service.as_deref().unwrap_or_default())).collect();
            let req = NetworkConnectRequest { container: Some(r.id.clone()), endpoint_config: Some(EndpointSettings { aliases: Some(aliases), ..Default::default() }) };
            if let Err(e) = app.docker.connect_network(net, req).await
                && !e.to_string().contains("already exists")
            {
                println!("  ! could not attach {} to {net}: {e}", r.name);
            }
        }
    }
}

async fn leave_overlays(app: &App, service_id: &str, overlays: &[(String, Vec<String>)]) {
    for r in runtime::replicas(app, service_id).await.unwrap_or_default() {
        for (net, _) in overlays {
            let _ = app.docker.disconnect_network(net, NetworkDisconnectRequest { container: Some(r.id.clone()), force: Some(true) }).await;
        }
    }
}

/// Run a command with `input` on stdin and return its stdout; errors carry stderr only.
async fn pipe(mut cmd: tokio::process::Command, input: &str) -> Result<Vec<u8>> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("running docker")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// Containers carrying a label (`key=value`), running ones only unless `all`.
async fn labeled(docker: &Docker, label: &str, all: bool) -> Result<Vec<String>> {
    let filters = HashMap::from([("label".to_string(), vec![label.to_string()])]);
    let list = docker.list_containers(Some(ListContainersOptions { all, filters: Some(filters), ..Default::default() })).await?;
    Ok(list.into_iter().filter_map(|c| c.id).collect())
}

async fn swarm_replicas(service: &str) -> Result<u64> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["service", "inspect", "--format", "{{.Spec.Mode.Replicated.Replicas}}", "--", service]);
    let out = crate::proc::output(cmd).await?;
    out.trim().parse().with_context(|| format!("{service} is not a replicated service"))
}

async fn scale(docker: &Docker, service: &str, n: u64, fast: bool) -> Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["service", "scale", "--detach", "--", &format!("{service}={n}")]);
    crate::proc::output(cmd).await?;
    let label = format!("com.docker.swarm.service.name={service}");
    let started = std::time::Instant::now();
    loop {
        let running = labeled(docker, &label, false).await?;
        if running.len() as u64 == n {
            return Ok(());
        }
        // The old proxy drains for up to 10 s; every second of that is a second of failed requests.
        if fast && n == 0 && started.elapsed() > Duration::from_secs(1) {
            for id in &running {
                let _ = docker.kill_container(id, None::<bollard::query_parameters::KillContainerOptions>).await;
            }
        }
        if started.elapsed() > Duration::from_secs(180) {
            bail!("{service} did not reach {n} running containers within 3 minutes");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// What to restore later: the old copy's replica count or running containers.
async fn record(docker: &Docker, old: &Old) -> Result<Stopped> {
    Ok(match old {
        Old::None => Stopped::Containers { ids: vec![], networks: vec![] },
        Old::Swarm { service } => Stopped::Swarm { service: service.clone(), replicas: swarm_replicas(service).await? },
        Old::Labeled { label } => Stopped::Containers { ids: labeled(docker, label, false).await?, networks: vec![] },
        Old::Container { name } => {
            let running = docker.inspect_container(name, None::<bollard::query_parameters::InspectContainerOptions>).await.ok().and_then(|c| c.state?.running);
            Stopped::Containers { ids: if running == Some(true) { vec![name.clone()] } else { vec![] }, networks: vec![] }
        }
    })
}

/// Stop the old copy and wait until it is gone (safe to repeat). `fast` gives it one second
/// instead of a graceful shutdown.
async fn halt(app: &App, old: &Old, fast: bool) -> Result<()> {
    let t = if fast { 1 } else { 30 };
    match old {
        Old::None => {}
        Old::Swarm { service } => scale(&app.docker, service, 0, fast).await?,
        Old::Labeled { label } => {
            for id in labeled(&app.docker, label, false).await? {
                runtime::stop(app, "local", &id, t).await?;
            }
        }
        Old::Container { name } => runtime::stop(app, "local", name, t).await?,
    }
    Ok(())
}

/// Subnets a Compose file pins (`networks.<name>.ipam.config[].subnet`).
fn pinned_subnets(yaml: &str) -> Vec<String> {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap_or_default();
    doc.get("networks")
        .and_then(|n| n.as_mapping())
        .into_iter()
        .flat_map(|n| n.values())
        .filter_map(|n| n.get("ipam")?.get("config")?.as_sequence())
        .flatten()
        .filter_map(|c| c.get("subnet")?.as_str().map(String::from))
        .collect()
}

/// The stopped old stack's networks keep the subnets Tugport's copy pins, and Docker refuses
/// overlapping pools. Record those networks in the journal, then remove them; `start_old`
/// creates them again.
async fn release_subnets(app: &App, j: &mut Journal, key: &str, subnets: &[String]) -> Result<()> {
    let Some(Stopped::Containers { ids, .. }) = moved_mut(j, key)?.stopped.clone() else { return Ok(()) };
    if subnets.is_empty() {
        return Ok(());
    }
    let mut names = BTreeSet::new();
    for id in &ids {
        let c = app.docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await?;
        names.extend(c.network_settings.and_then(|n| n.networks).into_iter().flat_map(|n| n.into_keys()));
    }
    for name in names {
        let Ok(n) = app.docker.inspect_network(&name, None::<bollard::query_parameters::InspectNetworkOptions>).await else { continue };
        let ipam = n.ipam.clone().unwrap_or_default();
        if !ipam.config.iter().flatten().any(|c| c.subnet.as_ref().is_some_and(|s| subnets.contains(s))) {
            continue;
        }
        if let Some(Stopped::Containers { networks, .. }) = &mut moved_mut(j, key)?.stopped
            && !networks.iter().any(|r| r.name == name)
        {
            networks.push(NetworkCreateRequest {
                name: name.clone(),
                driver: n.driver,
                internal: n.internal,
                attachable: n.attachable,
                ipam: Some(ipam),
                enable_ipv4: n.enable_ipv4,
                enable_ipv6: n.enable_ipv6,
                options: n.options,
                labels: n.labels,
                ..Default::default()
            });
            save(app, j)?;
        }
        println!("{key}: removing the old network {name}, which holds a subnet the stack pins");
        app.docker.remove_network(&name).await.with_context(|| format!("removing the old network {name}"))?;
    }
    Ok(())
}

/// Start an old copy again. Problems are printed, not returned: undoing continues.
async fn start_old(docker: &Docker, s: &Stopped) {
    let res = match s {
        Stopped::Swarm { service, replicas } => scale(docker, service, *replicas, false).await,
        Stopped::Containers { ids, networks } => {
            recreate_networks(docker, ids, networks).await;
            start_all(docker, ids).await
        }
    };
    if let Err(e) = res {
        println!("  ! could not start {s:?} again: {e:#}");
    }
}

/// Create the networks `release_subnets` removed and attach the old containers again with the
/// aliases and addresses they had (a container still names a removed network and cannot start).
async fn recreate_networks(docker: &Docker, ids: &[String], networks: &[NetworkCreateRequest]) {
    for req in networks {
        if docker.inspect_network(&req.name, None::<bollard::query_parameters::InspectNetworkOptions>).await.is_err()
            && let Err(e) = docker.create_network(req.clone()).await
        {
            println!("  ! could not create the network {} again: {e}", req.name);
            continue;
        }
        for id in ids {
            let Ok(c) = docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await else { continue };
            let Some(old) = c.network_settings.and_then(|n| n.networks).and_then(|mut n| n.remove(&req.name)) else { continue };
            let endpoint = EndpointSettings { aliases: old.aliases, ipam_config: old.ipam_config, links: old.links, ..Default::default() };
            let connect = NetworkConnectRequest { container: Some(id.clone()), endpoint_config: Some(endpoint) };
            if let Err(e) = docker.connect_network(&req.name, connect).await {
                println!("  ! could not attach {id} to {}: {e}", req.name);
            }
        }
    }
}

/// Start containers, retrying the ones that fail while others still come up: a container
/// sharing another's PID or network namespace starts only after that one runs.
async fn start_all(docker: &Docker, ids: &[String]) -> Result<()> {
    let mut left: Vec<&String> = ids.iter().collect();
    loop {
        let mut failed = vec![];
        let mut last = None;
        for id in &left {
            match docker.start_container(id, None::<bollard::query_parameters::StartContainerOptions>).await {
                Ok(()) | Err(bollard::errors::Error::DockerResponseServerError { status_code: 304, .. }) => {}
                Err(e) => {
                    failed.push(*id);
                    last = Some(e);
                }
            }
        }
        match last {
            None => return Ok(()),
            Some(e) if failed.len() == left.len() => return Err(e.into()),
            Some(_) => left = failed,
        }
    }
}

/// Stop the old proxy; Tugport's proxy is waiting for ports 80/443 and binds within half a second.
async fn switch(app: &AppState, plan: &Plan, j: &mut Journal) -> Result<()> {
    if plan.proxy == Old::None {
        return Ok(());
    }
    // cutover() already waited for the server to load the imported routes.
    server_running()?;
    remember(app, j, |j| Ok(&mut j.proxy), &plan.proxy).await?;
    println!("Switching the proxy");
    halt(app, &plan.proxy, true).await?;
    let port = std::env::var("TUGPORT_HTTP_PORT").unwrap_or_else(|_| "80".into());
    let probe = "tugport-migration.invalid";
    if !answers(&format!("http://127.0.0.1:{port}/"), Some(probe), Duration::from_secs(30)).await {
        restore(app, j, |j| Ok(&mut j.proxy), None).await?;
        bail!("Tugport's proxy did not take over port {port} within 30 seconds, so the old proxy was started again");
    }
    println!("Tugport now serves ports 80 and 443");
    Ok(())
}

/// A running server reloads routes and certificates every 30 seconds.
async fn wait_for_reload(j: &Journal) {
    let wait = j.imported_at + 32 - util::now();
    if wait > 0 {
        println!("Waiting {wait}s for Tugport to load the imported domains and certificates");
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
    }
}

/// Poll a URL until Tugport answers it: its proxy's page for `host`, or else its webhook
/// listener's answer to an unknown token.
async fn answers(url: &str, host: Option<&str>, timeout: Duration) -> bool {
    let Ok(client) = util::http(2).redirect(reqwest::redirect::Policy::none()).build() else { return false };
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let mut req = client.get(url);
        if let Some(h) = host {
            req = req.header("host", h);
        }
        if let Ok(r) = req.send().await {
            let body = r.text().await.unwrap_or_default();
            if body.contains(host.unwrap_or("unknown deploy hook")) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn server_running() -> Result<()> {
    if server_pids().is_empty() {
        bail!("Tugport is not running; start it (systemctl start tugport) and run the migration again. Its proxy takes over ports 80 and 443 at the switch");
    }
    Ok(())
}

/// Processes running `tugport server`.
fn server_pids() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return vec![] };
    // Containers' processes show up in /proc too; only this host's own count.
    let ours = std::fs::read_link("/proc/self/ns/pid").ok();
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter(|e| ours.is_some() && std::fs::read_link(e.path().join("ns/pid")).ok() == ours)
        .filter(|e| {
            let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            let mut args = cmd.split(|b| *b == 0);
            args.next().is_some_and(|a| a.ends_with(b"tugport")) && args.next() == Some(b"server")
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// Stop Tugport so the old proxy can have ports 80/443 back. Returns whether systemd ran it.
async fn stop_server() -> bool {
    let systemd = crate::proc::succeeds(std::process::Command::new("systemctl").args(["stop", "tugport"]));
    // However it runs, it must be gone before the old proxy can bind.
    for attempt in 0..100 {
        if server_pids().is_empty() {
            break;
        }
        if attempt == 25 || (!systemd && attempt == 0) {
            for pid in server_pids() {
                let _ = std::process::Command::new("kill").arg(&pid).status();
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    systemd
}

async fn rollback(app: &AppState, yes: bool) -> Result<()> {
    let Some(mut j) = load(app)? else {
        println!("There is no migration to roll back.");
        return Ok(());
    };
    println!("Rolling back the {} migration: {} services go back to the old panel.", j.panel, j.services.len());
    if !yes && !confirm("Continue?")? {
        bail!("nothing was changed");
    }
    // While moving back, old apps reach the moved databases by their old names again.
    for m in j.services.iter().filter(|m| m.swap && m.deployed) {
        join_overlays(app, &m.id, &m.overlays).await;
    }
    let keys = |j: &Journal, swap: bool| j.services.iter().filter(|m| m.swap == swap).map(|m| m.key.clone()).collect::<Vec<_>>();
    for key in keys(&j, false) {
        restore(app, &mut j, |j| Ok(&mut moved_mut(j, &key)?.stopped), Some(&format!("{key}: starting the old copy"))).await?;
    }
    let mut restart_systemd = false;
    if j.proxy.is_some() {
        println!("Switching the proxy back");
        restart_systemd = stop_server().await;
        if !server_pids().is_empty() {
            bail!("Tugport still runs and holds ports 80 and 443; stop it and run the rollback again");
        }
        restore(app, &mut j, |j| Ok(&mut j.proxy), None).await?;
    }
    for key in keys(&j, true).into_iter().rev() {
        let m = moved_mut(&mut j, &key)?;
        m.deployed = false;
        if let Some(svc) = store::service(app, &m.id)? {
            println!("{key}: stopping Tugport's copy");
            runtime::remove_service(app, &svc, false).await?;
        }
        restore(app, &mut j, |j| Ok(&mut moved_mut(j, &key)?.stopped), Some(&format!("{key}: starting the old copy"))).await?;
    }
    restore(app, &mut j, |j| Ok(&mut j.old_panel), Some("Starting the old panel")).await?;
    // Only what the migration changed: later changes (a private panel, trusted proxies...) stay.
    if let Some(before) = j.settings_before.take() {
        let mut settings = app.settings();
        settings.acme_email = before.acme_email;
        settings.panel_domain = before.panel_domain;
        settings.legacy_listen = before.legacy_listen;
        app.db.set_setting("instance", &settings)?;
    }
    delete_imported(app, &j).await?;
    app.db.del_state(JOURNAL)?;
    if restart_systemd {
        let _ = std::process::Command::new("systemctl").args(["start", "tugport"]).status();
    }
    println!("Rolled back. The old panel runs again{}.", if restart_systemd { "; Tugport is back to waiting for ports 80/443" } else { "" });
    Ok(())
}

/// The rows the import made; volumes stay.
async fn delete_imported(app: &AppState, j: &Journal) -> Result<()> {
    for m in &j.services {
        if let Some(svc) = store::service(app, &m.id)? {
            runtime::remove_service(app, &svc, false).await?;
            app.db.exec("DELETE FROM services WHERE id = ?1", [&svc.id])?;
        }
    }
    for host in &j.certs {
        app.db.exec("DELETE FROM certificates WHERE host = ?1 AND source = 'imported'", [host])?;
    }
    for p in j.provider.iter().chain(j.git_providers.values()) {
        app.db.exec("DELETE FROM git_providers WHERE id = ?1", [p])?;
    }
    for host in &j.registries {
        app.db.exec("DELETE FROM registries WHERE host = ?1", [host])?;
    }
    for id in &j.projects {
        if let Some(p) = store::project(app, id)?
            && store::services(app, Some(id))?.is_empty()
        {
            app.db.exec("DELETE FROM projects WHERE id = ?1", [id])?;
            runtime::remove_project_network(app, &p).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod git_url_tests {
    use super::*;

    #[test]
    fn tokens_leave_git_urls() {
        assert_eq!(
            credentialed_git("https://easypanel:abc123token@git.example.com/owner/repo.git"),
            Some(("https://git.example.com".into(), "owner/repo".into(), "abc123token".into()))
        );
        assert_eq!(credentialed_git("https://ghp_x@github.com/o/r").map(|c| c.2), Some("ghp_x".into()));
        assert_eq!(credentialed_git("https://github.com/o/r.git"), None);
        assert_eq!(redact_userinfo("https://u:p@h.example/o/r"), "https://h.example/o/r");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_pinned_subnets() {
        let yaml = "services:\n  a:\n    image: x\nnetworks:\n  ci:\n    ipam:\n      config:\n        - subnet: 172.31.250.0/24\n  plain: {}\n  other:\n";
        assert_eq!(pinned_subnets(yaml), vec!["172.31.250.0/24".to_string()]);
        assert!(pinned_subnets("services: {}").is_empty());
        assert!(pinned_subnets("{not yaml").is_empty());
    }

    #[test]
    fn names_and_mentions() {
        assert_eq!(tugport_name("cas_ban_bot"), "cas-ban-bot");
        assert_eq!(tugport_name("__nope__"), "nope");
        assert_eq!(tugport_name("2fa"), "s-2fa");
        assert_eq!(tugport_name("Web.App"), "web-app");
        assert!(util::valid_name(&tugport_name("x".repeat(60).as_str())));
        assert!(mentions("mongodb://u:p@bots_banofbot-db:27017/x", "bots_banofbot-db"));
        assert!(!mentions("mongodb://u:p@bots_banofbot-db-2:27017/x", "bots_banofbot-db"));
        assert!(!mentions("https://x.bots_banofbot-db.com", "bots_banofbot-db"));
        assert_eq!(image_tag("easypanel/easypanel:2.38.0@sha256:abc"), "2.38.0");
        assert_eq!(image_tag("localhost:5000/coolify"), "latest");
    }
}
