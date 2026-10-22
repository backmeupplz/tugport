//! Autoscaling: a service's replicas follow its CPU use or request rate between `min` and
//! `max`. More replicas start as soon as they are needed; extras go after five quiet minutes.

use crate::app::{App, AppState};
use crate::model::Autoscale;
use crate::{runtime, store, util};
use std::collections::HashMap;
use std::time::Duration;

const TICK_S: u64 = 30;
const UP_COOLDOWN_S: i64 = 60;
const DOWN_AFTER_S: i64 = 300;

fn bounds(a: &Autoscale) -> (u32, u32) {
    let min = a.min.max(1);
    (min, a.max.max(min))
}

/// How many replicas a service runs: its autoscaled count, or the fixed `replicas`.
pub fn replicas(app: &App, s: &store::Service) -> u32 {
    match &s.spec.autoscale {
        Some(a) => {
            let (min, max) = bounds(a);
            let current = app.db.get_state(&format!("scale:{}", s.id)).ok().flatten().and_then(|v| v.parse().ok());
            current.unwrap_or(min).clamp(min, max)
        }
        None => s.spec.replicas.max(1),
    }
}

/// Replicas the load calls for: total CPU (percent of one core, all replicas) over the
/// per-replica target, or requests per second over theirs, whichever wants more.
pub fn wanted(a: &Autoscale, running: u32, cpu: Option<f64>, rps: Option<f64>) -> u32 {
    let need = |load: Option<f64>, target: Option<u32>| Some((load? / target?.max(1) as f64).ceil() as u32);
    let want = need(cpu, a.cpu_percent).into_iter().chain(need(rps, a.rps)).max().unwrap_or(running);
    let (min, max) = bounds(a);
    want.clamp(min, max)
}

pub async fn run(app: AppState) {
    let mut seen: HashMap<String, (i64, u64)> = HashMap::new();
    let mut low_since: HashMap<String, i64> = HashMap::new();
    let mut changed_at: HashMap<String, i64> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(TICK_S)).await;
        let Ok(services) = store::services(&app, None) else { continue };
        let traffic = app.proxy.all_traffic();
        let now = util::now();
        for s in services {
            let Some(cfg) = &s.spec.autoscale else { continue };
            if s.kind != "app" || crate::nodes::stopped(&app, &s.id) || app.deployer.busy(&s.id) {
                continue;
            }
            let Some(live) = runtime::live_deploy(&app, &s.id) else { continue };
            let running = runtime::replicas(&app, &s.id).await.unwrap_or_default().into_iter().filter(|r| r.deploy == live && r.state == "running").count() as u32;
            if running == 0 {
                continue;
            }
            let cpu = crate::metrics::recent(&app, &format!("service:{}", s.id), "cpu", 120);
            let total = traffic.get(&s.id).map(|t| t.requests).unwrap_or(0);
            // Counters restart with the proxies; a drop means no reading this round.
            let rps = seen.insert(s.id.clone(), (now, total)).filter(|(at, before)| now > *at && total >= *before).map(|(at, before)| (total - before) as f64 / (now - at) as f64);
            let want = wanted(cfg, running, cpu, rps);
            let quiet = now - changed_at.get(&s.id).copied().unwrap_or(0) >= UP_COOLDOWN_S;
            let go = if want > running {
                low_since.remove(&s.id);
                quiet
            } else if want < running {
                now - *low_since.entry(s.id.clone()).or_insert(now) >= DOWN_AFTER_S
            } else {
                low_since.remove(&s.id);
                false
            };
            if !go {
                continue;
            }
            tracing::info!("scaling {}/{} from {running} to {want} replicas", s.project, s.name);
            match crate::deploy::scale(&app, &s, want).await {
                Ok(()) => {
                    app.emit("service.status", Some(&s.project_id), Some(&s.id), serde_json::json!({ "event": "scaled", "replicas": want }));
                    changed_at.insert(s.id.clone(), now);
                    low_since.remove(&s.id);
                }
                Err(e) => tracing::warn!("scaling {}/{}: {e:#}", s.project, s.name),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replicas_follow_load_within_bounds() {
        let a = Autoscale { min: 1, max: 5, cpu_percent: Some(70), rps: Some(100) };
        assert_eq!(wanted(&a, 2, Some(150.0), None), 3);
        assert_eq!(wanted(&a, 2, Some(10.0), Some(250.0)), 3);
        assert_eq!(wanted(&a, 2, Some(1000.0), None), 5);
        assert_eq!(wanted(&a, 3, Some(1.0), Some(0.0)), 1);
        // No readings: keep what runs, but inside the bounds.
        assert_eq!(wanted(&a, 2, None, None), 2);
        assert_eq!(wanted(&Autoscale { min: 3, ..a.clone() }, 1, None, None), 3);
    }
}
