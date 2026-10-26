//! Deploys: one queue per service, cancel-or-queue policy, zero-downtime rollouts,
//! automatic rollback when the new version is unhealthy.

use crate::app::{App, AppState};
use crate::model::{DeployPolicy, Source, Strategy};
use crate::runtime::{self, Replica};
use crate::secrets::Masker;
use crate::store::{self, Service};
use crate::util;
use anyhow::{Context, Result, anyhow};
use rusqlite::params;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct Deployer {
    queues: Mutex<HashMap<String, Queue>>,
}

impl Deployer {
    /// No deploy is running or waiting.
    pub fn idle(&self) -> bool {
        util::lock(&self.queues).is_empty()
    }

    /// A deploy of this service is running or waiting.
    pub fn busy(&self, service_id: &str) -> bool {
        util::lock(&self.queues).contains_key(service_id)
    }
}

#[derive(Default)]
struct Queue {
    running: Option<(String, CancellationToken)>,
    waiting: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Deploy {
    pub id: String,
    pub service_id: String,
    pub status: String,
    pub trigger: String,
    pub actor: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
    pub image: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Built with no layer cache and freshly pulled base images.
    pub no_cache: bool,
}

const SELECT: &str = "SELECT id, service_id, status, trigger, actor, ref, commit_sha, commit_message, image, error, created_at, started_at, finished_at, no_cache FROM deploys";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Deploy> {
    Ok(Deploy {
        id: r.get(0)?,
        service_id: r.get(1)?,
        status: r.get(2)?,
        trigger: r.get(3)?,
        actor: r.get(4)?,
        git_ref: r.get(5)?,
        commit_sha: r.get(6)?,
        commit_message: r.get(7)?,
        image: r.get(8)?,
        error: r.get(9)?,
        created_at: r.get(10)?,
        started_at: r.get(11)?,
        finished_at: r.get(12)?,
        no_cache: r.get(13)?,
    })
}

pub fn get(app: &App, id: &str) -> Result<Option<Deploy>> {
    Ok(app.db.row(&format!("{SELECT} WHERE id = ?1"), [id], row)?)
}

pub fn recent(app: &App, service_id: &str, limit: u32) -> Result<Vec<Deploy>> {
    Ok(app.db.rows(&format!("{SELECT} WHERE service_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2"), params![service_id, limit], row)?)
}

/// The newest deploys of every service, newest first.
pub fn recent_all(app: &App, limit: u32) -> Result<Vec<Deploy>> {
    Ok(app.db.rows(&format!("{SELECT} ORDER BY created_at DESC, rowid DESC LIMIT ?1"), params![limit], row)?)
}

pub const FINAL: &[&str] = &["live", "failed", "cancelled", "superseded"];

#[derive(Default, Clone)]
pub struct Request {
    pub trigger: String,
    pub actor: Option<String>,
    pub git_ref: Option<String>,
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
    /// Deploy this exact image (rollbacks) instead of building or pulling.
    pub image: Option<String>,
    /// Build without the layer cache and pull base images again.
    pub no_cache: bool,
}

fn set_status(app: &App, d: &str, status: &str, error: Option<&str>) {
    let now = util::now();
    let res = app.db.exec(
        "UPDATE deploys SET status = ?1, error = COALESCE(?2, error),
           started_at = CASE WHEN ?1 IN ('building', 'deploying') AND started_at IS NULL THEN ?3 ELSE started_at END,
           finished_at = CASE WHEN ?1 IN ('live', 'failed', 'cancelled', 'superseded') THEN ?3 ELSE finished_at END
         WHERE id = ?4",
        params![status, error, now, d],
    );
    if let Err(e) = res {
        tracing::error!("updating deploy {d}: {e}");
    }
}

fn announce(app: &App, s: &Service, d: &str, status: &str) {
    app.emit("deploy", Some(&s.project_id), Some(&s.id), serde_json::json!({ "deploy_id": d, "status": status }));
}

/// Set the status and tell the service's watchers.
fn transition(app: &App, s: &Service, d: &str, status: &str, error: Option<&str>) {
    set_status(app, d, status, error);
    announce(app, s, d, status);
}

/// Queue a deploy. With the cancel policy a running deploy is cancelled; either way
/// only the newest waiting deploy is kept.
pub fn enqueue(app: &AppState, s: &Service, req: Request) -> Result<String> {
    let id = util::id();
    app.db.exec(
        "INSERT INTO deploys (id, service_id, status, trigger, actor, ref, commit_sha, commit_message, image, created_at, no_cache) VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![id, s.id, req.trigger, req.actor, req.git_ref, req.commit_sha, req.commit_message, req.image, util::now(), req.no_cache],
    )?;
    announce(app, s, &id, "queued");
    let policy = s.spec.deploy_policy.unwrap_or(app.settings().deploy_policy);
    let mut queues = util::lock(&app.deployer.queues);
    let q = queues.entry(s.id.clone()).or_default();
    if let Some((running, token)) = &q.running {
        if policy == DeployPolicy::Cancel {
            tracing::info!("deploy {id} cancels running deploy {running}");
            token.cancel();
        }
        if let Some(old) = q.waiting.replace(id.clone()) {
            transition(app, s, &old, "superseded", Some("a newer deploy was queued"));
        }
        return Ok(id);
    }
    let token = CancellationToken::new();
    q.running = Some((id.clone(), token.clone()));
    drop(queues);
    tokio::spawn(worker(app.clone(), s.id.clone(), id.clone(), token));
    Ok(id)
}

/// Cancel a running or waiting deploy.
pub fn cancel(app: &App, deploy_id: &str) -> bool {
    let mut queues = util::lock(&app.deployer.queues);
    for q in queues.values_mut() {
        if let Some((id, token)) = &q.running
            && id == deploy_id {
                token.cancel();
                return true;
            }
        if q.waiting.as_deref() == Some(deploy_id) {
            q.waiting = None;
            set_status(app, deploy_id, "cancelled", None);
            return true;
        }
    }
    false
}

pub fn cancel_all(app: &App, service_id: &str) {
    let mut queues = util::lock(&app.deployer.queues);
    if let Some(q) = queues.get_mut(service_id) {
        if let Some(w) = q.waiting.take() {
            set_status(app, &w, "cancelled", None);
        }
        if let Some((_, t)) = &q.running {
            t.cancel();
        }
    }
}

async fn worker(app: AppState, service_id: String, mut deploy_id: String, mut token: CancellationToken) {
    loop {
        run(&app, &service_id, &deploy_id, token.clone()).await;
        let mut queues = util::lock(&app.deployer.queues);
        let q = queues.entry(service_id.clone()).or_default();
        match q.waiting.take() {
            Some(next) => {
                token = CancellationToken::new();
                q.running = Some((next.clone(), token.clone()));
                deploy_id = next;
            }
            None => {
                queues.remove(&service_id);
                return;
            }
        }
    }
}

/// Writes a deploy's log to disk and streams it to watchers, with secrets masked.
pub struct DeployLog {
    app: AppState,
    deploy_id: String,
    project_id: String,
    service_id: String,
    file: Mutex<Option<std::fs::File>>,
    masker: Masker,
}

impl DeployLog {
    fn new(app: &AppState, s: &Service, deploy_id: &str) -> Self {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(log_path(app, deploy_id)).ok();
        DeployLog {
            app: app.clone(),
            deploy_id: deploy_id.into(),
            project_id: s.project_id.clone(),
            service_id: s.id.clone(),
            file: Mutex::new(file),
            masker: Masker::for_service(app, s),
        }
    }

    pub fn line(&self, text: &str) {
        let text = self.masker.mask(text.trim_end());
        let ts = util::now_ms();
        if let Some(f) = util::lock(&self.file).as_mut() {
            let _ = writeln!(f, "{ts}\t{text}");
        }
        self.app.emit(
            "deploy.log",
            Some(&self.project_id),
            Some(&self.service_id),
            serde_json::json!({ "deploy_id": self.deploy_id, "ts": ts, "line": text }),
        );
    }
}

fn log_path(app: &App, deploy_id: &str) -> std::path::PathBuf {
    app.paths.deploy_logs().join(format!("{deploy_id}.log"))
}

pub fn read_log(app: &App, deploy_id: &str) -> Vec<(i64, String)> {
    read_log_file(&log_path(app, deploy_id))
}

/// The `ts<TAB>line` lines of a log file (deploy and backup run logs).
pub fn read_log_file(path: &std::path::Path) -> Vec<(i64, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(t, s)| (t.parse().unwrap_or(0), s.to_string())))
        .collect()
}

async fn run(app: &AppState, service_id: &str, deploy_id: &str, token: CancellationToken) {
    let Ok(Some(s)) = store::service(app, service_id) else { return };
    let Ok(Some(d)) = get(app, deploy_id) else { return };
    if FINAL.contains(&d.status.as_str()) {
        return;
    }
    let log = DeployLog::new(app, &s, deploy_id);
    transition(app, &s, deploy_id, "building", None);
    crate::git::report(app, &s, &d, "pending", "Deploying").await;
    let started = std::time::Instant::now();
    let result = tokio::select! {
        r = execute(app, &s, &d, &log) => r,
        _ = token.cancelled() => Err(anyhow!("cancelled")),
    };
    match result {
        Ok(()) => {
            log.line(&format!("Live after {:.1}s.", started.elapsed().as_secs_f32()));
            transition(app, &s, deploy_id, "live", None);
            crate::git::report(app, &s, &d, "success", "Live").await;
            crate::notify::event(app, "deploy.succeeded", &format!("{}/{} deployed", s.project, s.name), &summary(&d), Some(&s)).await;
        }
        Err(e) if token.is_cancelled() => {
            log.line("Cancelled.");
            cleanup_after_cancel(app, &s, deploy_id).await;
            transition(app, &s, deploy_id, "cancelled", None);
            crate::git::report(app, &s, &d, "failure", "Cancelled").await;
            tracing::debug!("deploy {deploy_id} cancelled: {e}");
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log.line(&format!("Deploy failed: {msg}"));
            transition(app, &s, deploy_id, "failed", Some(&msg));
            crate::git::report(app, &s, &d, "failure", "Deploy failed").await;
            crate::notify::event(app, "deploy.failed", &format!("{}/{} failed to deploy", s.project, s.name), &msg, Some(&s)).await;
        }
    }
}

fn summary(d: &Deploy) -> String {
    let Some(sha) = &d.commit_sha else { return d.trigger.clone() };
    let short = sha.get(..7).unwrap_or(sha);
    match &d.commit_message {
        Some(m) => format!("{short} {}", m.lines().next().unwrap_or("")),
        None => short.to_string(),
    }
}

/// After a cancel: drop this deploy's containers unless it already took traffic.
async fn cleanup_after_cancel(app: &App, s: &Service, deploy_id: &str) {
    let live = runtime::live_deploy(app, &s.id);
    let went_live = live.as_deref() == Some(deploy_id);
    for r in runtime::replicas(app, &s.id).await.unwrap_or_default() {
        // A deploy that went live keeps its containers; one that never took traffic loses them.
        if went_live != (r.deploy == deploy_id) {
            let _ = runtime::stop_and_remove(app, &r, 10).await;
        } else if !went_live && live.as_deref() == Some(r.deploy.as_str()) && r.state != "running" && !crate::nodes::stopped(app, &s.id) {
            // A stop-first rollout cancelled after stopping the live version: start it again.
            let _ = runtime::start(app, &r.node, &r.id).await;
        }
    }
}

async fn execute(app: &AppState, s: &Service, d: &Deploy, log: &DeployLog) -> Result<()> {
    if s.kind == "compose" {
        return crate::compose::deploy(app, s, d, log).await;
    }
    let image = match (&d.image, &s.spec.source) {
        (Some(image), _) => {
            let what = match d.trigger.as_str() {
                "rollback" => "Rolling back to",
                "migration" => "Taking over the running image",
                _ => "Deploying",
            };
            log.line(&format!("{what} {image}"));
            if app.docker.inspect_image(image).await.is_err() {
                runtime::pull(app, image, &|l| log.line(l)).await?;
            }
            image.clone()
        }
        (None, Source::Image { image }) => {
            log.line(&format!("Pulling {image}"));
            runtime::pull(app, image, &|l| log.line(l)).await?;
            image.clone()
        }
        (None, Source::Git { .. } | Source::Dockerfile { .. }) => crate::builder::build(app, s, d, log).await?,
        (None, Source::Compose { .. }) => return Err(anyhow!("this service is a Compose stack")),
    };
    app.db.exec("UPDATE deploys SET image = ?1 WHERE id = ?2", params![image, d.id])?;
    transition(app, s, &d.id, "deploying", None);
    rollout(app, s, &d.id, &image, log).await
}

fn health_timeout(s: &Service) -> Duration {
    Duration::from_secs(s.spec.rollout.health_timeout_s.max(10).into())
}

fn containers(n: usize) -> String {
    format!("{n} container{}", if n == 1 { "" } else { "s" })
}

/// Replace the running replicas with `image`.
async fn rollout(app: &AppState, s: &Service, deploy_id: &str, image: &str, log: &DeployLog) -> Result<()> {
    let network = runtime::ensure_network(app, &s.project).await?;
    let img = runtime::image_info(app, image).await?;
    let old: Vec<Replica> = runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.deploy != deploy_id).collect();
    let n = crate::autoscale::replicas(app, s);
    let strategy = if !s.spec.ports.is_empty() || s.kind == "database" || s.spec.host_network { Strategy::StopFirst } else { s.spec.rollout.strategy };
    let timeout = health_timeout(s);
    let nodes = crate::nodes::place(app, s, n, &[]).await?;
    let mut distinct: Vec<&crate::nodes::NodeHandle> = vec![];
    for node in &nodes {
        if !distinct.iter().any(|d| d.id == node.id) {
            distinct.push(node);
        }
    }
    if crate::nodes::mesh_enabled(app) {
        log.line(&format!("Running on {}", distinct.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    for node in &distinct {
        crate::nodes::ship_image(app, node, s, image, &|l| log.line(l)).await?;
    }

    if strategy == Strategy::StopFirst {
        if !old.is_empty() {
            log.line(&format!("Stopping the old version ({})", containers(old.len())));
        }
        // A failed stop must not end the rollout between stopping the old version and starting the
        // new one: `stop` kills what it could not stop, and if the old version still runs, the new
        // one fails to start or to become healthy and the old one keeps serving.
        for r in &old {
            if let Err(e) = runtime::stop(app, &r.node, &r.id, 30).await {
                log.line(&format!("Warning: could not stop {}: {e:#}", r.name));
            }
        }
    }

    // (node, container id, container name)
    let mut created: Vec<(String, String, String)> = vec![];
    let result: Result<()> = async {
        for (i, node) in (1..).zip(&nodes) {
            let id = runtime::create_replica(app, node, s, deploy_id, i, image, &img, &network, i == 1).await?;
            created.push((node.id.clone(), id.clone(), runtime::container_name(s, deploy_id, i)));
            runtime::start(app, &node.id, &id).await?;
        }
        log.line(&format!("Started {}; checking health", containers(created.len())));
        for (node, id, _) in &created {
            if let Err(e) = runtime::wait_healthy(app, s, node, id, &network, timeout, img.has_healthcheck, &|l| log.line(l)).await {
                for line in runtime::tail_logs(app, node, id, 30).await {
                    log.line(&format!("  | {line}"));
                }
                return Err(e.context("the new version is not healthy"));
            }
        }
        Ok(())
    }
    .await;

    if let Err(e) = result {
        for (node, id, name) in &created {
            let _ = runtime::remove(app, node, id, name).await;
        }
        if strategy == Strategy::StopFirst && !old.is_empty() {
            log.line("Starting the previous version again");
            for r in &old {
                if let Err(e) = runtime::start(app, &r.node, &r.id).await {
                    log.line(&format!("Warning: could not start {} again: {e:#}", r.name));
                }
            }
        } else if !old.is_empty() {
            log.line("The previous version keeps serving traffic.");
        }
        return Err(e);
    }

    runtime::set_live_deploy(app, &s.id, deploy_id)?;
    app.db.set_state(&format!("exposed:{}", s.id), &serde_json::to_string(&img.exposed)?)?;
    app.db.set_state(&format!("nodes:{}", s.id), &distinct.iter().map(|d| d.id.as_str()).collect::<Vec<_>>().join(","))?;
    app.db.exec("DELETE FROM state WHERE key IN (?1, ?2)", [format!("stopped:{}", s.id), format!("asleep:{}", s.id)])?;
    crate::sleep::mark_active(app, &s.id);
    crate::proxy::reload(app);
    let serving_old = old.iter().filter(|r| r.state == "running").count();
    let drain = if strategy == Strategy::StartFirst && serving_old > 0 && !store::domains(app, Some(&s.id))?.is_empty() { s.spec.rollout.drain_s } else { 0 };
    if drain > 0 {
        log.line(&format!("Traffic switched; the old version finishes its requests for {drain}s, then goes away"));
    }
    // The new version serves now, so the deploy is done: the old one drains and is removed in the
    // background, and the next deploy can start meanwhile.
    let mut retired: Vec<String> = old.into_iter().map(|r| r.deploy).collect();
    retired.sort();
    retired.dedup();
    if !retired.is_empty() {
        tokio::spawn(retire(app.clone(), s.id.clone(), retired, drain));
    }
    Ok(())
}

/// After `drain_s`, remove the containers of the `retired` deploys, retrying for about half an hour.
/// Only those deploys: by then a newer deploy may be starting containers of its own. Replicas the
/// autoscaler added to the old version meanwhile belong to a retired deploy too.
async fn retire(app: AppState, service_id: String, retired: Vec<String>, drain_s: u32) {
    tokio::time::sleep(Duration::from_secs(drain_s.into())).await;
    for wait in [0, 30, 60, 120, 240, 480, 900] {
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let result: Result<()> = async {
            for r in runtime::replicas(&app, &service_id).await?.iter().filter(|r| retired.contains(&r.deploy)) {
                runtime::stop_and_remove(&app, r, 30).await.with_context(|| format!("removing the old container {}", r.name))?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => return,
            Err(e) => tracing::warn!("{e:#} (service {service_id}); retrying"),
        }
    }
    tracing::error!("gave up removing the old containers of {service_id}; the next deploy removes them");
}

/// Run `want` replicas of the live version: start more next to the running ones, or take
/// the newest extras out of routing, let them finish their requests, and remove them.
pub async fn scale(app: &AppState, s: &Service, want: u32) -> Result<()> {
    let live = runtime::live_deploy(app, &s.id).ok_or_else(|| anyhow!("{} is not deployed", s.name))?;
    let image = get(app, &live)?.and_then(|d| d.image).ok_or_else(|| anyhow!("the live version has no image"))?;
    let mut reps: Vec<Replica> = runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.deploy == live).collect();
    let n = reps.len() as u32;
    app.db.set_state(&format!("scale:{}", s.id), &want.to_string())?;
    if want > n {
        let network = runtime::ensure_network(app, &s.project).await?;
        let img = runtime::image_info(app, &image).await?;
        let existing: Vec<String> = reps.iter().map(|r| r.node.clone()).collect();
        let mut next = reps.iter().map(|r| r.replica).max().unwrap_or(0);
        for node in crate::nodes::place(app, s, want - n, &existing).await? {
            next += 1;
            crate::nodes::ship_image(app, &node, s, &image, &|_| {}).await?;
            runtime::start_healthy(app, &node, s, &live, next, &image, &img, &network, health_timeout(s)).await.context("a new replica did not become healthy")?;
        }
        crate::proxy::reload(app);
    } else if want < n {
        reps.sort_by_key(|r| std::cmp::Reverse(r.replica));
        reps.truncate((n - want) as usize);
        util::lock(&app.proxy.draining).extend(reps.iter().map(|r| r.id.clone()));
        crate::proxy::rebuild_now(app).await;
        tokio::time::sleep(Duration::from_secs(s.spec.rollout.drain_s.into())).await;
        let removed = async {
            for r in &reps {
                runtime::stop_and_remove(app, r, 30).await?;
            }
            anyhow::Ok(())
        }
        .await;
        let mut draining = util::lock(&app.proxy.draining);
        for r in &reps {
            draining.remove(&r.id);
        }
        removed?;
    }
    let nodes: std::collections::BTreeSet<String> = runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.deploy == live).map(|r| r.node).collect();
    app.db.set_state(&format!("nodes:{}", s.id), &nodes.into_iter().collect::<Vec<_>>().join(","))?;
    Ok(())
}

/// Images of earlier successful deploys, newest first, for rollbacks.
pub fn previous_images(app: &App, service_id: &str, limit: u32) -> Result<Vec<(String, String)>> {
    Ok(app.db.rows(
        "SELECT id, image FROM deploys WHERE service_id = ?1 AND status = 'live' AND image IS NOT NULL ORDER BY created_at DESC LIMIT ?2",
        params![service_id, limit],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Deploys left mid-way by a crash or restart: mark them failed.
pub fn recover_interrupted(app: &App) -> Result<()> {
    app.db.exec(
        "UPDATE deploys SET status = 'failed', error = 'Tugport restarted during this deploy', finished_at = ?1 WHERE status IN ('queued', 'building', 'deploying')",
        [util::now()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_shorten_any_sha() {
        let mut d = Deploy {
            id: "d".into(), service_id: "s".into(), status: "live".into(), trigger: "push".into(), actor: None, git_ref: None,
            commit_sha: Some("0123456789abcdef".into()), commit_message: Some("fix it\nbody".into()), image: None, error: None,
            created_at: 0, started_at: None, finished_at: None, no_cache: false,
        };
        assert_eq!(summary(&d), "0123456 fix it");
        d.commit_sha = Some("ééééé".into());
        d.commit_message = None;
        assert_eq!(summary(&d), "ééééé");
    }
}
