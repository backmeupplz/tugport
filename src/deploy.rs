//! Deploys: one queue per service, cancel-or-queue policy, zero-downtime rollouts,
//! automatic rollback when the new version is unhealthy.

use crate::app::{App, AppState};
use crate::model::{DeployPolicy, Source, Strategy};
use crate::runtime::{self, Replica};
use crate::secrets::Masker;
use crate::store::{self, Service};
use crate::util;
use anyhow::{Context, Result, anyhow};
use rusqlite::{OptionalExtension, params};
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
        crate::util::lock(&self.queues).is_empty()
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
}

const SELECT: &str = "SELECT id, service_id, status, trigger, actor, ref, commit_sha, commit_message, image, error, created_at, started_at, finished_at FROM deploys";

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
    })
}

pub fn get(app: &App, id: &str) -> Result<Option<Deploy>> {
    Ok(app.db.conn().query_row(&format!("{SELECT} WHERE id = ?1"), [id], row).optional()?)
}

pub fn recent(app: &App, service_id: &str, limit: u32) -> Result<Vec<Deploy>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(&format!("{SELECT} WHERE service_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2"))?;
    Ok(stmt.query_map(params![service_id, limit], row)?.collect::<rusqlite::Result<_>>()?)
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
}

fn set_status(app: &App, d: &str, status: &str, error: Option<&str>) {
    let now = util::now();
    let res = app.db.conn().execute(
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

/// Queue a deploy. With the cancel policy a running deploy is cancelled; either way
/// only the newest waiting deploy is kept.
pub fn enqueue(app: &AppState, s: &Service, req: Request) -> Result<String> {
    let id = util::id();
    app.db.conn().execute(
        "INSERT INTO deploys (id, service_id, status, trigger, actor, ref, commit_sha, commit_message, image, created_at) VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, s.id, req.trigger, req.actor, req.git_ref, req.commit_sha, req.commit_message, req.image, util::now()],
    )?;
    announce(app, s, &id, "queued");
    let policy = s.spec.deploy_policy.unwrap_or(app.settings().deploy_policy);
    let mut queues = crate::util::lock(&app.deployer.queues);
    let q = queues.entry(s.id.clone()).or_default();
    if let Some((running, token)) = &q.running {
        if policy == DeployPolicy::Cancel {
            tracing::info!("deploy {id} cancels running deploy {running}");
            token.cancel();
        }
        if let Some(old) = q.waiting.replace(id.clone()) {
            set_status(app, &old, "superseded", Some("a newer deploy was queued"));
            announce(app, s, &old, "superseded");
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
    let mut queues = crate::util::lock(&app.deployer.queues);
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
    let mut queues = crate::util::lock(&app.deployer.queues);
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
        let mut queues = crate::util::lock(&app.deployer.queues);
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
        let path = app.paths.deploy_logs().join(format!("{deploy_id}.log"));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
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
        if let Some(f) = crate::util::lock(&self.file).as_mut() {
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

pub fn read_log(app: &App, deploy_id: &str) -> Vec<(i64, String)> {
    let path = app.paths.deploy_logs().join(format!("{deploy_id}.log"));
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
    set_status(app, deploy_id, "building", None);
    announce(app, &s, deploy_id, "building");
    crate::git::report(app, &s, &d, "pending", "Deploying").await;
    let started = std::time::Instant::now();
    let result = tokio::select! {
        r = execute(app, &s, &d, &log) => r,
        _ = token.cancelled() => Err(anyhow!("cancelled")),
    };
    match result {
        Ok(()) => {
            log.line(&format!("Live after {:.1}s.", started.elapsed().as_secs_f32()));
            set_status(app, deploy_id, "live", None);
            announce(app, &s, deploy_id, "live");
            crate::git::report(app, &s, &d, "success", "Live").await;
            crate::notify::event(app, "deploy.succeeded", &format!("{}/{} deployed", s.project, s.name), &summary(&d), Some(&s)).await;
        }
        Err(e) if token.is_cancelled() => {
            log.line("Cancelled.");
            cleanup_after_cancel(app, &s, deploy_id).await;
            set_status(app, deploy_id, "cancelled", None);
            announce(app, &s, deploy_id, "cancelled");
            crate::git::report(app, &s, &d, "failure", "Cancelled").await;
            tracing::debug!("deploy {deploy_id} cancelled: {e}");
        }
        Err(e) => {
            let msg = format!("{e:#}");
            log.line(&format!("Deploy failed: {msg}"));
            set_status(app, deploy_id, "failed", Some(&msg));
            announce(app, &s, deploy_id, "failed");
            crate::git::report(app, &s, &d, "failure", "Deploy failed").await;
            crate::notify::event(app, "deploy.failed", &format!("{}/{} failed to deploy", s.project, s.name), &msg, Some(&s)).await;
        }
    }
}

fn summary(d: &Deploy) -> String {
    match (&d.commit_sha, &d.commit_message) {
        (Some(sha), Some(m)) => format!("{} {}", &sha[..sha.len().min(7)], m.lines().next().unwrap_or("")),
        (Some(sha), None) => sha[..sha.len().min(7)].to_string(),
        _ => d.trigger.clone(),
    }
}

/// After a cancel: drop this deploy's containers unless it already took traffic.
async fn cleanup_after_cancel(app: &App, s: &Service, deploy_id: &str) {
    let live = runtime::live_deploy(app, &s.id);
    for r in runtime::replicas(app, &s.id).await.unwrap_or_default() {
        let ours = r.deploy == deploy_id;
        if (live.as_deref() == Some(deploy_id) && !ours) || (live.as_deref() != Some(deploy_id) && ours) {
            let _ = runtime::stop_and_remove(app, &r, 10).await;
        }
    }
}

async fn execute(app: &AppState, s: &Service, d: &Deploy, log: &DeployLog) -> Result<()> {
    if s.kind == "compose" {
        return crate::compose::deploy(app, s, d, log).await;
    }
    let image = match (&d.image, &s.spec.source) {
        (Some(image), _) => {
            log.line(&format!("Rolling back to {image}"));
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
        (None, Source::Git { .. } | Source::Dockerfile { .. }) => {
            let built = crate::builder::build(app, s, d, log).await?;
            app.db.conn().execute(
                "UPDATE deploys SET commit_sha = ?1, commit_message = ?2 WHERE id = ?3",
                params![built.commit_sha, built.commit_message, d.id],
            )?;
            built.image
        }
        (None, Source::Compose { .. }) => return Err(anyhow!("this service is a Compose stack")),
    };
    app.db.conn().execute("UPDATE deploys SET image = ?1 WHERE id = ?2", params![image, d.id])?;
    set_status(app, &d.id, "deploying", None);
    announce(app, s, &d.id, "deploying");
    rollout(app, s, &d.id, &image, log).await
}

/// Replace the running replicas with `image`.
pub async fn rollout(app: &AppState, s: &Service, deploy_id: &str, image: &str, log: &DeployLog) -> Result<()> {
    let network = runtime::ensure_network(app, &s.project).await?;
    let img = runtime::image_info(app, image).await?;
    let old: Vec<Replica> = runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.deploy != deploy_id).collect();
    let n = s.spec.replicas.max(1);
    let strategy = if !s.spec.ports.is_empty() || s.kind == "database" || s.spec.host_network { Strategy::StopFirst } else { s.spec.rollout.strategy };
    let timeout = Duration::from_secs(s.spec.rollout.health_timeout_s.max(10) as u64);

    if strategy == Strategy::StopFirst {
        if !old.is_empty() {
            log.line(&format!("Stopping the old version ({} container{})", old.len(), if old.len() == 1 { "" } else { "s" }));
        }
        for r in &old {
            runtime::stop(app, &r.id, 30).await?;
        }
    }

    let mut created = vec![];
    let result: Result<()> = async {
        for i in 1..=n {
            let id = runtime::create_replica(app, s, deploy_id, i, image, &img, &network, i == 1).await?;
            runtime::start(app, &id).await?;
            created.push(id);
        }
        log.line(&format!("Started {n} container{}; checking health", if n == 1 { "" } else { "s" }));
        for id in &created {
            if let Err(e) = runtime::wait_healthy(app, s, id, &network, timeout, img.has_healthcheck, &|l| log.line(l)).await {
                for line in runtime::tail_logs(app, id, 30).await {
                    log.line(&format!("  | {line}"));
                }
                return Err(e.context("the new version is not healthy"));
            }
        }
        Ok(())
    }
    .await;

    if let Err(e) = result {
        for id in &created {
            let name = runtime::replicas(app, &s.id).await.ok().and_then(|rs| rs.into_iter().find(|r| &r.id == id).map(|r| r.name)).unwrap_or_default();
            let _ = runtime::remove(app, id, &name).await;
        }
        if strategy == Strategy::StopFirst && !old.is_empty() {
            log.line("Starting the previous version again");
            for r in &old {
                let _ = runtime::start(app, &r.id).await;
            }
        } else if !old.is_empty() {
            log.line("The previous version keeps serving traffic.");
        }
        return Err(e);
    }

    runtime::set_live_deploy(app, &s.id, deploy_id)?;
    crate::proxy::reload(app);
    let serving_old = old.iter().filter(|r| r.state == "running").count();
    if strategy == Strategy::StartFirst && serving_old > 0 && !store::domains(app, Some(&s.id))?.is_empty() {
        let drain = s.spec.rollout.drain_s;
        log.line(&format!("Traffic switched; letting the old version finish requests for {drain}s"));
        tokio::time::sleep(Duration::from_secs(drain as u64)).await;
    }
    for r in &old {
        runtime::stop_and_remove(app, r, 30).await.with_context(|| format!("removing {}", r.name))?;
    }
    Ok(())
}

/// Images of earlier successful deploys, newest first, for rollbacks.
pub fn previous_images(app: &App, service_id: &str, limit: u32) -> Result<Vec<(String, String)>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, image FROM deploys WHERE service_id = ?1 AND status = 'live' AND image IS NOT NULL ORDER BY created_at DESC LIMIT ?2",
    )?;
    Ok(stmt.query_map(params![service_id, limit], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
}

/// Deploys left mid-way by a crash or restart: mark them failed.
pub fn recover_interrupted(app: &App) -> Result<()> {
    app.db.conn().execute(
        "UPDATE deploys SET status = 'failed', error = 'Tugport restarted during this deploy', finished_at = ?1 WHERE status IN ('queued', 'building', 'deploying')",
        [util::now()],
    )?;
    Ok(())
}
