//! Alerts: thresholds that hold for a while, crash loops and out-of-memory kills.

use crate::app::{App, AppState};
use crate::metrics::{Snapshot, percent};
use crate::{runtime, store, util};
use rusqlite::params;
use std::collections::{HashMap, VecDeque};

/// Threshold alerts for a server's snapshot (`node` None = the panel's own server).
pub async fn evaluate(app: &App, node: Option<&crate::nodes::NodeHandle>, s: &Snapshot) {
    let a = app.settings().alerts;
    let memory = percent(s.memory.used, s.memory.total);
    let mut checks: Vec<(String, bool, String)> = vec![
        ("cpu".into(), s.cpu > a.cpu_percent, format!("CPU is at {:.0}%", s.cpu)),
        ("memory".into(), memory > a.memory_percent, format!("Memory is at {:.0}%", memory)),
    ];
    for d in &s.disks {
        let p = percent(d.used, d.total);
        checks.push((format!("disk:{}", d.mount), p > a.disk_percent, format!("Disk {} is {:.0}% full", d.mount, p)));
    }
    for g in &s.gpus {
        if let Some(t) = g.temp {
            checks.push((format!("gpu-temp:{}", g.index), t > a.gpu_temp_c, format!("GPU {} ({}) is at {:.0}°C", g.index, g.name, t)));
        }
    }
    let now = util::now();
    for (key, firing, msg) in checks {
        let (key, msg) = match node {
            Some(n) => (format!("{}:{key}", n.id), format!("{}: {msg}", n.name)),
            None => (key, msg),
        };
        let row: Option<(i64, Option<i64>)> = app.db.row("SELECT since, last_notified FROM alerts WHERE key = ?1", [&key], |r| Ok((r.get(0)?, r.get(1)?))).ok().flatten();
        match (firing, row) {
            (true, None) => {
                let _ = app.db.exec("INSERT INTO alerts (key, firing, since, message) VALUES (?1, 1, ?2, ?3)", params![key, now, msg]);
            }
            (true, Some((since, None))) if now - since >= a.for_minutes as i64 * 60 => {
                crate::notify::event(app, "alert", &format!("Alert: {msg}"), &format!("It has lasted {} minutes.", (now - since) / 60), None).await;
                let _ = app.db.exec("UPDATE alerts SET last_notified = ?1, message = ?2 WHERE key = ?3", params![now, msg, key]);
                app.emit("alert", None, None, serde_json::json!({ "key": key, "message": msg, "firing": true }));
            }
            (false, Some((_, notified))) => {
                if notified.is_some() {
                    crate::notify::event(app, "alert", &format!("Resolved: {key}"), "Back to normal.", None).await;
                    app.emit("alert", None, None, serde_json::json!({ "key": key, "firing": false }));
                }
                let _ = app.db.exec("DELETE FROM alerts WHERE key = ?1", [&key]);
            }
            _ => {}
        }
    }
}

/// Crash loops (3+ exits in 10 minutes) and out-of-memory kills of managed containers.
pub async fn watch_events(app: AppState) {
    crate::nodes::for_each_node(app, |app, node| watch_on(app, node.docker.clone())).await;
}

async fn watch_on(app: AppState, docker: bollard::Docker) {
    use futures_util::StreamExt;
    let mut exits: HashMap<String, VecDeque<i64>> = HashMap::new();
    let mut quiet_until: HashMap<String, i64> = HashMap::new();
    // Containers being stopped on purpose (Docker sends them a kill first): their exit is no crash.
    let mut stopping: HashMap<String, i64> = HashMap::new();
    let mut filters = runtime::managed();
    filters.insert("type".into(), vec!["container".into()]);
    let mut events = docker.events(Some(bollard::query_parameters::EventsOptions { filters: Some(filters), ..Default::default() }));
    while let Some(Ok(e)) = events.next().await {
        let action = e.action.unwrap_or_default();
        let status = matches!(action.as_str(), "start" | "die" | "oom" | "pause" | "unpause") || action.starts_with("health_status");
        if !status && action != "kill" {
            continue;
        }
        let attrs = e.actor.and_then(|a| a.attributes).unwrap_or_default();
        let Some(service_id) = attrs.get(runtime::L_SERVICE).cloned() else { continue };
        let name = attrs.get("name").cloned().unwrap_or_default();
        let Ok(Some(s)) = store::service(&app, &service_id) else { continue };
        let now = util::now();
        match action.as_str() {
            "kill" => {
                stopping.retain(|_, t| now - *t < 120);
                stopping.insert(name.clone(), now);
            }
            "oom" => {
                crate::notify::event(&app, "service.oom", &format!("{}/{} ran out of memory", s.project, s.name), &format!("Container {name} was killed for using too much memory."), Some(&s)).await;
            }
            "die" => {
                let code = attrs.get("exitCode").cloned().unwrap_or_default();
                let deliberate = stopping.remove(&name).is_some_and(|t| now - t < 120);
                if code != "0" && !deliberate {
                    *util::lock(&app.metrics.restarts).entry(service_id.clone()).or_default() += 1;
                    let q = exits.entry(service_id.clone()).or_default();
                    q.push_back(now);
                    while q.front().is_some_and(|t| now - t > 600) {
                        q.pop_front();
                    }
                    if q.len() >= 3 && quiet_until.get(&service_id).is_none_or(|t| *t < now) {
                        quiet_until.insert(service_id.clone(), now + 1800);
                        crate::notify::event(
                            &app,
                            "service.crashing",
                            &format!("{}/{} keeps crashing", s.project, s.name),
                            &format!("It exited {} times in 10 minutes (last exit code {code}).", q.len()),
                            Some(&s),
                        )
                        .await;
                    }
                }
            }
            _ => {}
        }
        if status {
            status_changed(&app, &s, &action, &name);
        }
    }
}

/// Tell watchers the service's state after one of its containers changed.
fn status_changed(app: &AppState, s: &store::Service, event: &str, container: &str) {
    let (app, s, event, container) = (app.clone(), s.clone(), event.to_string(), container.to_string());
    tokio::spawn(async move {
        let st = runtime::service_status(&app, &s).await;
        let data = serde_json::json!({ "event": event, "container": container, "state": st["state"], "running": st["running"], "desired": st["desired"] });
        app.emit("service.status", Some(&s.project_id), Some(&s.id), data);
    });
}
