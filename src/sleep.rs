//! Scale to zero: services with `sleep` set stop when idle and wake on the next request.
//! Requests arriving at a sleeping service wait for one shared wake-up; other servers'
//! proxies send them to the panel, which does the waking.

use crate::app::{App, AppState};
use crate::proxy::Upstream;
use crate::{runtime, store, util};
use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Waking and putting to sleep take turns per service.
static TURNS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);
/// When this process started: services count as busy until then.
static STARTED: LazyLock<i64> = LazyLock::new(util::now);

fn turn(service_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    util::lock(&TURNS).entry(service_id.to_string()).or_default().clone()
}

pub fn is_asleep(app: &App, service_id: &str) -> bool {
    app.db.state::<String>(&format!("asleep:{service_id}")).is_some()
}

/// Deploys and wake-ups restart the idle clock.
pub fn mark_active(app: &App, service_id: &str) {
    let _ = app.db.set_state(&format!("active:{service_id}"), &util::now().to_string());
}

/// Start a sleeping service and return where to send the request once it answers.
pub async fn wake(app: &App, service_id: &str, port: u16) -> Result<Vec<Upstream>> {
    let gate = turn(service_id);
    let _turn = gate.lock().await;
    let s = store::service(app, service_id)?.ok_or_else(|| anyhow!("this service no longer exists"))?;
    let reps = runtime::current(app, &s).await?;
    if reps.is_empty() {
        bail!("there is nothing to start; deploy the service first");
    }
    let started = Instant::now();
    if is_asleep(app, &s.id) {
        tracing::info!("waking {}/{}", s.project, s.name);
        for r in reps.iter().filter(|r| r.state != "running") {
            runtime::start(app, &r.node, &r.id).await?;
        }
        app.db.del_state(&format!("asleep:{}", s.id))?;
        mark_active(app, &s.id);
        app.emit("service.status", Some(&s.project_id), Some(&s.id), serde_json::json!({ "event": "wake", "state": "running" }));
    }
    let timeout = Duration::from_secs(s.spec.rollout.health_timeout_s.max(30) as u64);
    let proxies = crate::nodes::proxy_addresses(app);
    loop {
        let mut up = vec![];
        for r in runtime::current(app, &s).await?.iter().filter(|r| r.state == "running") {
            let Some(addr) = r.ip.as_deref() else { continue };
            let Ok(ip) = addr.parse::<std::net::IpAddr>() else { continue };
            if !runtime::probe(app, &r.node, addr, port, None, false).await {
                continue;
            }
            let local = Upstream::Local(SocketAddr::new(ip, port));
            up.push(if r.node == "local" { local } else { proxies.get(&r.node).copied().map(Upstream::Node).unwrap_or(local) });
        }
        if !up.is_empty() {
            if started.elapsed() > Duration::from_millis(50) {
                // Other servers learn the new routes before their proxies see a forwarded request.
                crate::proxy::rebuild_now(app).await;
                tracing::info!("{}/{} answered after {:.1}s", s.project, s.name, started.elapsed().as_secs_f32());
            }
            return Ok(up);
        }
        if started.elapsed() > timeout {
            bail!("the service did not answer within {}s", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Put idle services to sleep (checked every 30 seconds).
pub async fn run(app: AppState) {
    LazyLock::force(&STARTED);
    crate::util::every(30, "scale to zero", || sleep_idle(app.clone())).await
}

async fn sleep_idle(app: AppState) -> Result<()> {
    let app = &app;
    let traffic = app.proxy.all_traffic();
    for s in store::services(app, None)? {
        let Some(cfg) = &s.spec.sleep else { continue };
        // Only web services can be woken by a request.
        if s.kind != "app" || is_asleep(app, &s.id) || crate::nodes::stopped(app, &s.id) || app.deployer.busy(&s.id) || store::domains(app, Some(&s.id))?.is_empty() {
            continue;
        }
        let active: i64 = app.db.get_state(&format!("active:{}", s.id))?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let last = traffic.get(&s.id).map(|t| t.last_at).unwrap_or(0).max(active).max(*STARTED);
        let idle = util::now() - last;
        if idle < cfg.idle_minutes.max(1) as i64 * 60 {
            continue;
        }
        let gate = turn(&s.id);
        let _turn = gate.lock().await;
        // Asleep first, so new requests wait for a wake-up instead of reaching a stopping container.
        app.db.set_state(&format!("asleep:{}", s.id), &util::now().to_string())?;
        crate::proxy::rebuild_now(app).await;
        for r in runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.state == "running") {
            runtime::stop(app, &r.node, &r.id, 30).await?;
        }
        tracing::info!("{}/{} is asleep after {} minutes without requests", s.project, s.name, idle / 60);
        app.emit("service.status", Some(&s.project_id), Some(&s.id), serde_json::json!({ "event": "sleep", "state": "asleep" }));
    }
    Ok(())
}
