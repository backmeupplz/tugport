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

pub mod coolify;
pub mod easypanel;

use crate::app::{App, AppState, Settings};
use crate::model::{DomainOptions, ServiceSpec, Source};
use crate::store::{self, Service};
use crate::{deploy, git, runtime, util};
use anyhow::{Context, Result, anyhow, bail};
use bollard::Docker;
use bollard::models::{EndpointSettings, NetworkConnectRequest, NetworkDisconnectRequest};
use bollard::query_parameters::{ListContainersOptions, StopContainerOptions};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
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
#[derive(Debug, Serialize)]
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
    let _ = rustls::crypto::ring::default_provider().install_default();
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
pub struct Plan {
    pub panel: &'static str,
    pub version: String,
    pub projects: Vec<NewProject>,
    pub services: Vec<NewService>,
    pub certs: Vec<Cert>,
    pub github_token: Option<String>,
    /// (host, user, password) for private images.
    pub registries: Vec<(String, String, String)>,
    pub acme_email: Option<String>,
    pub panel_domain: Option<String>,
    /// Tugport also serves its API here after the switch, so old webhook URLs keep working.
    pub legacy_listen: Option<String>,
    pub proxy: Old,
    pub old_panel: Old,
    /// Instance-wide things that do not move.
    pub notes: Vec<String>,
}

pub struct NewProject {
    pub name: String,
    pub vars: BTreeMap<String, String>,
}

pub struct NewService {
    /// The old panel's name (`project/service`); the journal key.
    pub key: String,
    pub project: String,
    pub name: String,
    pub kind: &'static str,
    pub spec: ServiceSpec,
    pub env: BTreeMap<String, String>,
    pub files: Vec<(String, String)>,
    pub domains: Vec<NewDomain>,
    /// The old deploy webhook token; its URL keeps working.
    pub hook: Option<String>,
    /// The image the old copy runs, deployed as-is so nothing is rebuilt during the move.
    pub image: Option<String>,
    pub commit: Option<String>,
    pub old: Old,
    pub cutover: Cutover,
    /// Old networks Tugport's copy joins while old services still run, with the names it answers to.
    pub overlays: Vec<(String, Vec<String>)>,
    /// What does not carry over exactly.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cutover {
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

pub struct NewDomain {
    pub host: String,
    pub path: String,
    pub port: u16,
    pub container: Option<String>,
    pub https: bool,
    pub options: DomainOptions,
}

pub struct Cert {
    pub host: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// How to stop and start the old copy of something.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Old {
    #[default]
    None,
    /// A swarm service, stopped by scaling it to zero.
    Swarm { service: String },
    /// Plain containers carrying this label (`key=value`).
    Labeled { label: String },
    /// One container by name.
    Container { name: String },
}

/// Placeholder provider id in planned specs; import swaps in the GitHub provider it creates.
pub const GITHUB: &str = "$github";

/// Tugport names are `[a-z][a-z0-9-]*`: `cas_ban_bot` becomes `cas-ban-bot`.
pub fn tugport_name(old: &str) -> String {
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
pub fn link_projects(services: &mut [NewService], names: impl Fn(&NewService) -> Vec<String>, text: impl Fn(&NewService) -> String) {
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
pub fn mentions(text: &str, host: &str) -> bool {
    let part = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    text.match_indices(host).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + host.len()..].chars().next();
        !before.is_some_and(|c| part(c) || c == '.') && !after.is_some_and(part)
    })
}

/// What a migration did, so reruns resume and rollback can undo it.
#[derive(Serialize, Deserialize, Default)]
struct Journal {
    panel: String,
    projects: Vec<String>,
    services: Vec<Moved>,
    certs: Vec<String>,
    provider: Option<String>,
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
    Containers { ids: Vec<String> },
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
    Ok(Arc::new(App {
        paths,
        db,
        keys: crate::crypto::Keyring::new(&master),
        docker,
        events: tokio::sync::broadcast::channel(4096).0,
        deployer: deploy::Deployer::default(),
        build_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        proxy: crate::proxy::Proxy::new()?,
        logs: tokio::sync::broadcast::channel(16).0,
        selflog,
        metrics: crate::metrics::Metrics::default(),
    }))
}

async fn migrate(panel: &'static str, dry_run: bool, yes: bool) -> Result<()> {
    let docker = Docker::connect_with_local_defaults().context("connecting to Docker")?;
    let plan = match panel {
        "easypanel" => easypanel::plan(&docker).await?,
        _ => coolify::plan(&docker).await?,
    };
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
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!("pass --yes to run without a terminal");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
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
    let attention: Vec<String> = moved.iter().flat_map(|s| s.notes.iter().map(move |n| format!("{}: {n}", s.key))).collect();
    if !attention.is_empty() {
        println!("\nNeeds attention:");
        for a in attention {
            println!("  - {a}");
        }
    }
    let skipped: Vec<String> = plan
        .services
        .iter()
        .filter(|s| s.cutover == Cutover::Unsupported)
        .map(|s| format!("{}: {}", s.key, s.notes.join("; ")))
        .chain(plan.notes.iter().cloned())
        .collect();
    if !skipped.is_empty() {
        println!("\nNot moved:");
        for s in skipped {
            println!("  - {s}");
        }
    }
    let hooks = moved.iter().filter(|s| s.hook.is_some()).count();
    println!("\nCertificates: {} imported, so nothing is re-issued", plan.certs.len());
    if hooks > 0 {
        println!("Deploy webhooks: {hooks} old URLs keep working{}", plan.legacy_listen.as_ref().map(|l| format!(" (Tugport answers on {l} after the switch)")).unwrap_or_default());
    }
    let count = |f: &dyn Fn(&&&NewService) -> bool| moved.iter().filter(f).count();
    println!(
        "Cutover: databases move first ({}), then apps start next to their old copies ({}), then services that must not run twice move ({}). \
         Each moved database and service is down for a few seconds; everything else only while the proxy switches (about a second).",
        count(&|s| s.cutover == Cutover::Swap && s.kind == "database"),
        count(&|s| s.cutover == Cutover::Parallel),
        count(&|s| s.cutover == Cutover::Swap && s.kind != "database"),
    );
}

fn kind_label(s: &NewService) -> String {
    match &s.spec.database {
        Some(d) => crate::databases::engine_name(d.engine).to_string(),
        None => s.kind.to_string(),
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
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

    if let (Some(token), None) = (&plan.github_token, &j.provider) {
        let config = serde_json::json!({ "token": token, "webhook_secret": util::token(40) });
        j.provider = Some(git::save_provider(app, None, git::Kind::GithubToken, &format!("GitHub (from {})", plan.panel), "https://github.com", &config)?);
        save(app, j)?;
    }
    for (host, user, password) in &plan.registries {
        let exists: bool = app.db.conn().query_row("SELECT COUNT(*) > 0 FROM registries WHERE host = ?1", [host], |r| r.get(0))?;
        if !exists {
            app.db.conn().execute(
                "INSERT INTO registries (id, host, username, password, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![util::id(), host, user, app.keys.seal_str(&format!("registry:{host}"), password)?, util::now()],
            )?;
            j.registries.push(host.clone());
            save(app, j)?;
        }
    }
    for p in &plan.projects {
        let project = match store::project(app, &p.name)? {
            Some(p) => p,
            None => {
                let id = util::id();
                app.db.conn().execute("INSERT INTO projects (id, name, description, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, p.name, format!("Moved from {}", plan.panel), util::now()])?;
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

fn import_service(app: &App, s: &NewService, provider: Option<&str>, j: &mut Journal) -> Result<()> {
    let existing = store::service(app, &format!("{}.{}", s.project, s.name))?;
    let pos = j.services.iter().position(|m| m.key == s.key);
    match (existing, pos.map(|i| &j.services[i])) {
        (Some(_), Some(m)) if m.imported => return Ok(()),
        // Half imported by an interrupted run: start over.
        (Some(e), Some(_)) => {
            app.db.conn().execute("DELETE FROM services WHERE id = ?1", [&e.id])?;
        }
        (Some(_), None) => bail!("Tugport already has a service {}.{}; rename or delete it and run the migration again", s.project, s.name),
        (None, _) => {}
    }
    let project = store::project(app, &s.project)?.ok_or_else(|| anyhow!("project {} is missing", s.project))?;
    let mut spec = s.spec.clone();
    if let Source::Git { provider: p, .. } = &mut spec.source
        && p.as_deref() == Some(GITHUB)
    {
        *p = provider.map(String::from);
    }
    let id = util::id();
    let now = util::now();
    let moved = Moved { key: s.key.clone(), id: id.clone(), imported: false, deployed: false, swap: s.cutover == Cutover::Swap, overlays: s.overlays.clone(), stopped: None };
    match pos {
        Some(i) => j.services[i] = moved,
        None => j.services.push(moved),
    }
    save(app, j)?;
    app.db.conn().execute(
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
        let added = app.db.conn().execute(
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
    let current: Option<Option<i64>> = app.db.conn().query_row("SELECT not_after FROM certificates WHERE host = ?1 AND cert_pem IS NOT NULL", [&c.host], |r| r.get(0)).optional()?;
    if current.flatten().is_some_and(|have| not_after.is_none_or(|n| have >= n)) {
        return Ok(());
    }
    app.db.conn().execute(
        "INSERT INTO certificates (host, cert_pem, key_pem, not_after, source, updated_at) VALUES (?1, ?2, ?3, ?4, 'imported', ?5)
         ON CONFLICT(host) DO UPDATE SET cert_pem = excluded.cert_pem, key_pem = excluded.key_pem, not_after = excluded.not_after, source = 'imported', last_error = NULL, next_attempt_at = NULL, updated_at = excluded.updated_at",
        params![c.host, c.cert_pem, app.keys.seal_str(&format!("cert:{}", c.host), &c.key_pem)?, not_after, util::now()],
    )?;
    if !j.certs.contains(&c.host) {
        j.certs.push(c.host.clone());
    }
    Ok(())
}

async fn cutover(app: &AppState, plan: &Plan, j: &mut Journal) -> Result<()> {
    if plan.proxy != Old::None && j.proxy.is_none() && server_pids().is_empty() {
        bail!("Tugport is not running; start it (systemctl start tugport) and run the migration again. Its proxy takes over ports 80 and 443 at the switch");
    }
    let moved: Vec<&NewService> = plan.services.iter().filter(|s| j.services.iter().any(|m| m.key == s.key && m.imported)).collect();
    let mut projects: Vec<&str> = moved.iter().map(|s| s.project.as_str()).collect();
    projects.sort();
    projects.dedup();
    for p in projects {
        runtime::ensure_network(app, p).await?;
    }
    let (databases, rest): (Vec<&NewService>, Vec<&NewService>) = moved.iter().partition(|s| s.kind == "database");
    for s in databases.iter().filter(|s| s.cutover == Cutover::Swap) {
        swap(app, s, j).await?;
    }
    for s in rest.iter().filter(|s| s.cutover == Cutover::Parallel) {
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

    for s in rest.iter().filter(|s| s.cutover == Cutover::Parallel) {
        if moved_mut(j, &s.key)?.stopped.is_none() {
            moved_mut(j, &s.key)?.stopped = Some(record(&app.docker, &s.old).await?);
            save(app, j)?;
        }
        halt(&app.docker, &s.old, false).await?;
    }
    if plan.old_panel != Old::None {
        if j.old_panel.is_none() {
            j.old_panel = Some(record(&app.docker, &plan.old_panel).await?);
            save(app, j)?;
        }
        println!("Stopping the old panel");
        halt(&app.docker, &plan.old_panel, false).await?;
    }
    if let Some(listen) = &plan.legacy_listen {
        let mut settings = app.settings();
        settings.legacy_listen = Some(listen.clone());
        app.db.set_setting("instance", &settings)?;
        let port = listen.rsplit(':').next().unwrap_or("3000");
        if answers(&format!("http://127.0.0.1:{port}/api/setup"), None, Duration::from_secs(30)).await {
            println!("Old deploy webhook URLs now reach Tugport on {listen}");
        } else {
            println!("  ! Tugport does not answer on {listen} yet; restart it to serve the old webhook URLs");
        }
    }
    // Every old consumer is gone: drop the old networks so removing the old panel later breaks nothing.
    for m in j.services.iter().filter(|m| m.swap) {
        leave_overlays(app, &m.id, &m.overlays).await;
    }
    // Hosts without an imported certificate failed their challenges while the old proxy held
    // port 80; retry them now instead of after the backoff.
    app.db.conn().execute("DELETE FROM state WHERE key LIKE 'acme_attempts:%'", [])?;
    app.db.conn().execute("UPDATE certificates SET next_attempt_at = NULL WHERE cert_pem IS NULL", [])?;
    j.done = true;
    save(app, j)
}

fn moved_mut<'a>(j: &'a mut Journal, key: &str) -> Result<&'a mut Moved> {
    j.services.iter_mut().find(|m| m.key == key).ok_or_else(|| anyhow!("{key} is not in the journal"))
}

/// Stop the old copy, then start Tugport's on the same data; on failure the old one comes back.
async fn swap(app: &AppState, s: &NewService, j: &mut Journal) -> Result<()> {
    let m = moved_mut(j, &s.key)?.clone();
    if !m.deployed {
        // Record what to restore before stopping anything; stop again on reruns, since an
        // interrupted run may have recorded without stopping.
        if m.stopped.is_none() {
            moved_mut(j, &s.key)?.stopped = Some(record(&app.docker, &s.old).await?);
            save(app, j)?;
        }
        println!("{}: stopping the old copy", s.key);
        halt(&app.docker, &s.old, false).await?;
        println!("{}: starting on Tugport", s.key);
        let svc = store::service(app, &m.id)?.ok_or_else(|| anyhow!("{} vanished", s.key))?;
        if let Err(e) = start(app, &svc, s).await {
            println!("{}: failed, starting the old copy again", s.key);
            let _ = runtime::remove_service(app, &svc, false).await;
            if let Some(stopped) = moved_mut(j, &s.key)?.stopped.take() {
                start_old(&app.docker, &stopped).await;
            }
            save(app, j)?;
            return Err(e.context(format!("moving {}", s.key)));
        }
        moved_mut(j, &s.key)?.deployed = true;
        save(app, j)?;
    }
    join_overlays(app, &m.id, &s.overlays).await;
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
    cmd.args(["service", "inspect", "--format", "{{.Spec.Mode.Replicated.Replicas}}", service]);
    let out = crate::proc::output(cmd).await?;
    out.trim().parse().with_context(|| format!("{service} is not a replicated service"))
}

async fn scale(docker: &Docker, service: &str, n: u64, fast: bool) -> Result<()> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["service", "scale", "--detach", &format!("{service}={n}")]);
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
        Old::None => Stopped::Containers { ids: vec![] },
        Old::Swarm { service } => Stopped::Swarm { service: service.clone(), replicas: swarm_replicas(service).await? },
        Old::Labeled { label } => Stopped::Containers { ids: labeled(docker, label, false).await? },
        Old::Container { name } => {
            let running = docker.inspect_container(name, None::<bollard::query_parameters::InspectContainerOptions>).await.ok().and_then(|c| c.state?.running);
            Stopped::Containers { ids: if running == Some(true) { vec![name.clone()] } else { vec![] } }
        }
    })
}

/// Stop the old copy and wait until it is gone (safe to repeat). `fast` gives it one second
/// instead of a graceful shutdown.
async fn halt(docker: &Docker, old: &Old, fast: bool) -> Result<()> {
    match old {
        Old::None => {}
        Old::Swarm { service } => scale(docker, service, 0, fast).await?,
        Old::Labeled { label } => {
            for id in labeled(docker, label, false).await? {
                docker.stop_container(&id, Some(StopContainerOptions { t: Some(if fast { 1 } else { 30 }), ..Default::default() })).await?;
            }
        }
        Old::Container { name } => match docker.stop_container(name, Some(StopContainerOptions { t: Some(if fast { 1 } else { 30 }), ..Default::default() })).await {
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 304 | 404, .. }) | Ok(()) => {}
            Err(e) => return Err(e.into()),
        },
    }
    Ok(())
}

/// Start an old copy again. Problems are printed, not returned: undoing continues.
async fn start_old(docker: &Docker, s: &Stopped) {
    let res = match s {
        Stopped::Swarm { service, replicas } => scale(docker, service, *replicas, false).await,
        Stopped::Containers { ids } => {
            let mut res = Ok(());
            for id in ids {
                if let Err(e) = docker.start_container(id, None::<bollard::query_parameters::StartContainerOptions>).await
                    && !e.to_string().contains("304")
                {
                    res = Err(e.into());
                }
            }
            res
        }
    };
    if let Err(e) = res {
        println!("  ! could not start {s:?} again: {e:#}");
    }
}

/// Stop the old proxy; Tugport's proxy is waiting for ports 80/443 and binds within half a second.
async fn switch(app: &AppState, plan: &Plan, j: &mut Journal) -> Result<()> {
    if plan.proxy == Old::None {
        return Ok(());
    }
    if server_pids().is_empty() {
        bail!("Tugport is not running; start it (systemctl start tugport) and run the migration again. Its proxy takes over ports 80 and 443 at the switch");
    }
    if j.proxy.is_none() {
        wait_for_reload(j).await;
        j.proxy = Some(record(&app.docker, &plan.proxy).await?);
        save(app, j)?;
    }
    println!("Switching the proxy");
    halt(&app.docker, &plan.proxy, true).await?;
    let port = std::env::var("TUGPORT_HTTP_PORT").unwrap_or_else(|_| "80".into());
    let probe = "tugport-migration.invalid";
    if !answers(&format!("http://127.0.0.1:{port}/"), Some(probe), Duration::from_secs(30)).await {
        if let Some(p) = j.proxy.take() {
            start_old(&app.docker, &p).await;
        }
        save(app, j)?;
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

/// Poll a URL until Tugport answers it. With `host`, the answer must be Tugport's proxy page for it.
async fn answers(url: &str, host: Option<&str>, timeout: Duration) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(2)).redirect(reqwest::redirect::Policy::none()).build() else { return false };
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let mut req = client.get(url);
        if let Some(h) = host {
            req = req.header("host", h);
        }
        if let Ok(r) = req.send().await {
            let ok = r.status().is_success();
            let body = r.text().await.unwrap_or_default();
            match host {
                Some(h) if body.contains(h) => return true,
                None if ok && body.contains("needs_setup") => return true,
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Processes running `tugport server`.
fn server_pids() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return vec![] };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
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
    let systemd = std::process::Command::new("systemctl")
        .args(["stop", "tugport"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
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
    for i in 0..j.services.len() {
        if !j.services[i].swap
            && let Some(stopped) = j.services[i].stopped.take()
        {
            println!("{}: starting the old copy", j.services[i].key);
            start_old(&app.docker, &stopped).await;
            save(app, &j)?;
        }
    }
    let mut restart_systemd = false;
    if let Some(proxy) = j.proxy.clone() {
        println!("Switching the proxy back");
        restart_systemd = stop_server().await;
        if !server_pids().is_empty() {
            bail!("Tugport still runs and holds ports 80 and 443; stop it and run the rollback again");
        }
        start_old(&app.docker, &proxy).await;
        j.proxy = None;
        save(app, &j)?;
    }
    for i in (0..j.services.len()).rev() {
        if !j.services[i].swap {
            continue;
        }
        let key = j.services[i].key.clone();
        if let Some(svc) = store::service(app, &j.services[i].id)? {
            println!("{key}: stopping Tugport's copy");
            runtime::remove_service(app, &svc, false).await?;
        }
        j.services[i].deployed = false;
        if let Some(stopped) = j.services[i].stopped.take() {
            println!("{key}: starting the old copy");
            start_old(&app.docker, &stopped).await;
        }
        save(app, &j)?;
    }
    if let Some(panel) = j.old_panel.take() {
        println!("Starting the old panel");
        start_old(&app.docker, &panel).await;
        save(app, &j)?;
    }
    if let Some(s) = j.settings_before.take() {
        app.db.set_setting("instance", &s)?;
    }
    for m in &j.services {
        if let Some(svc) = store::service(app, &m.id)? {
            runtime::remove_service(app, &svc, false).await?;
            app.db.conn().execute("DELETE FROM services WHERE id = ?1", [&svc.id])?;
        }
    }
    for host in &j.certs {
        app.db.conn().execute("DELETE FROM certificates WHERE host = ?1 AND source = 'imported'", [host])?;
    }
    if let Some(p) = &j.provider {
        app.db.conn().execute("DELETE FROM git_providers WHERE id = ?1", [p])?;
    }
    for host in &j.registries {
        app.db.conn().execute("DELETE FROM registries WHERE host = ?1", [host])?;
    }
    for id in &j.projects {
        if let Some(p) = store::project(app, id)?
            && store::services(app, Some(id))?.is_empty()
        {
            app.db.conn().execute("DELETE FROM projects WHERE id = ?1", [id])?;
            runtime::remove_project_network(app, &p).await;
        }
    }
    app.db.conn().execute("DELETE FROM state WHERE key = ?1", [JOURNAL])?;
    if restart_systemd {
        let _ = std::process::Command::new("systemctl").args(["start", "tugport"]).status();
    }
    println!("Rolled back. The old panel runs again{}.", if restart_systemd { "; Tugport is back to waiting for ports 80/443" } else { "" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
