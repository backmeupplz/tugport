//! Server and service metrics: sampled every 10 s, kept raw for a day and as 5-minute
//! averages/maxima for 30 days. Also drives threshold alerts.

use crate::app::{App, AppState};
use crate::runtime;
use crate::util;
use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

pub const STEP: i64 = 10;

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Snapshot {
    pub at: i64,
    pub cpu: f64,
    pub cores: Vec<f64>,
    pub load: [f64; 3],
    pub memory: Usage,
    pub swap: Usage,
    pub disks: Vec<DiskUsage>,
    pub disk_io: Rate,
    pub net: Rate,
    pub temp: Option<f64>,
    pub gpus: Vec<Gpu>,
    pub services: Vec<ServiceUsage>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Usage {
    pub used: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Rate {
    pub rx: f64,
    pub tx: f64,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct DiskUsage {
    pub mount: String,
    pub used: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Gpu {
    pub index: u32,
    pub name: String,
    pub util: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub temp: Option<f64>,
    pub power: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct ServiceUsage {
    pub id: String,
    /// Percent of one core, summed over replicas.
    pub cpu: f64,
    pub memory: u64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub restarts: u64,
}

#[derive(Default)]
pub struct Metrics {
    pub latest: Mutex<Snapshot>,
    /// Latest snapshot of each other server.
    pub nodes: Mutex<HashMap<String, Snapshot>>,
    /// Crashes per service since the last sample (from container events on every server).
    pub restarts: Mutex<HashMap<String, u64>>,
    series: Mutex<HashMap<(String, String, String), i64>>,
}

struct ContainerPrev {
    cpu_usec: u64,
    rx: u64,
    tx: u64,
    at: std::time::Instant,
}

/// Sample this server every 10 s. `keep` stores history and checks alerts (on the panel;
/// agents only report their latest snapshot).
pub async fn sample_loop(app: AppState, keep: bool) {
    let mut sys = sysinfo::System::new();
    let mut disks = sysinfo::Disks::new_with_refreshed_list();
    let mut nets = sysinfo::Networks::new_with_refreshed_list();
    let mut comps = sysinfo::Components::new_with_refreshed_list();
    let mut prev: HashMap<String, ContainerPrev> = HashMap::new();
    let mut last_rollup = 0;
    sys.refresh_cpu_usage();
    loop {
        tokio::time::sleep(Duration::from_secs(STEP as u64)).await;
        let at = util::now();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        disks.refresh(true);
        nets.refresh(true);
        comps.refresh(true);
        let cores: Vec<f64> = sys.cpus().iter().map(|c| c.cpu_usage() as f64).collect();
        let cpu = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f64>() / cores.len() as f64 };
        let load = sysinfo::System::load_average();
        let mut snap = Snapshot {
            at,
            cpu,
            cores,
            load: [load.one, load.five, load.fifteen],
            memory: Usage { used: sys.used_memory(), total: sys.total_memory() },
            swap: Usage { used: sys.used_swap(), total: sys.total_swap() },
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        for d in disks.list() {
            let mount = d.mount_point().to_string_lossy().to_string();
            let fs = d.file_system().to_string_lossy().to_string();
            if d.total_space() == 0 || fs.contains("overlay") || fs.contains("tmpfs") || !seen.insert(d.name().to_string_lossy().to_string()) {
                continue;
            }
            let u = d.usage();
            snap.disk_io.rx += u.read_bytes as f64 / STEP as f64;
            snap.disk_io.tx += u.written_bytes as f64 / STEP as f64;
            snap.disks.push(DiskUsage { mount, used: d.total_space() - d.available_space(), total: d.total_space() });
        }
        for (name, n) in nets.list() {
            if name == "lo" || name.starts_with("veth") || name.starts_with("br-") || name.starts_with("docker") {
                continue;
            }
            snap.net.rx += n.received() as f64 / STEP as f64;
            snap.net.tx += n.transmitted() as f64 / STEP as f64;
        }
        snap.temp = comps.list().iter().filter_map(|c| c.temperature()).map(|t| t as f64).fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
        snap.gpus = gpus().await;
        snap.services = containers(&app, &mut prev).await;

        if !keep {
            *crate::util::lock(&app.metrics.latest) = snap;
            continue;
        }
        let mut crashes = std::mem::take(&mut *crate::util::lock(&app.metrics.restarts));
        for sv in &mut snap.services {
            sv.restarts = crashes.remove(&sv.id).unwrap_or(0);
        }
        // Services crashing elsewhere count here too (charts add up every server).
        snap.services.extend(crashes.into_iter().map(|(id, restarts)| ServiceUsage { id, restarts, ..Default::default() }));
        if let Err(e) = store_for(&app, "local", &snap) {
            tracing::warn!("storing metrics: {e:#}");
        }
        crate::alerts::evaluate(&app, None, &snap).await;
        app.emit("metrics", None, None, serde_json::json!({ "at": at, "cpu": snap.cpu, "memory": snap.memory, "net": snap.net }));
        *crate::util::lock(&app.metrics.latest) = snap;
        if at - last_rollup >= 300 {
            if let Err(e) = rollup(&app) {
                tracing::warn!("metrics rollup: {e:#}");
            }
            last_rollup = at;
        }
    }
}

async fn gpus() -> Vec<Gpu> {
    let mut out = vec![];
    let nvidia = tokio::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw", "--format=csv,noheader,nounits"])
        .output()
        .await;
    if let Ok(o) = nvidia
        && o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
                if f.len() < 7 {
                    continue;
                }
                let num = |s: &str| s.parse::<f64>().ok();
                out.push(Gpu {
                    index: f[0].parse().unwrap_or(0),
                    name: f[1].to_string(),
                    util: num(f[2]).unwrap_or(0.0),
                    mem_used: (num(f[3]).unwrap_or(0.0) * 1048576.0) as u64,
                    mem_total: (num(f[4]).unwrap_or(0.0) * 1048576.0) as u64,
                    temp: num(f[5]),
                    power: num(f[6]),
                });
            }
            return out;
        }
    // AMD (and other DRM drivers that expose busy percent).
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return out };
    let mut cards: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("card") && !n.contains('-'))).collect();
    cards.sort();
    for (i, card) in cards.into_iter().enumerate() {
        let dev = card.join("device");
        let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok().and_then(|s| s.trim().parse::<f64>().ok());
        let Some(util) = read("gpu_busy_percent") else { continue };
        let hwmon = std::fs::read_dir(dev.join("hwmon")).ok().and_then(|mut r| r.next()).and_then(|e| e.ok()).map(|e| e.path());
        let hw = |f: &str| hwmon.as_ref().and_then(|h| std::fs::read_to_string(h.join(f)).ok()).and_then(|s| s.trim().parse::<f64>().ok());
        out.push(Gpu {
            index: i as u32,
            name: std::fs::read_to_string(dev.join("product_name")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "GPU".into()),
            util,
            mem_used: read("mem_info_vram_used").unwrap_or(0.0) as u64,
            mem_total: read("mem_info_vram_total").unwrap_or(0.0) as u64,
            temp: hw("temp1_input").map(|t| t / 1000.0),
            power: hw("power1_average").map(|p| p / 1_000_000.0),
        });
    }
    out
}

/// Per-service usage from cgroup v2 files (cheap), summed over replicas.
async fn containers(app: &App, prev: &mut HashMap<String, ContainerPrev>) -> Vec<ServiceUsage> {
    let filters = HashMap::from([("label".to_string(), vec![format!("{}=true", runtime::L_MANAGED)])]);
    let Ok(list) = app.docker.list_containers(Some(bollard::query_parameters::ListContainersOptions { filters: Some(filters), ..Default::default() })).await else {
        return vec![];
    };
    let mut per: HashMap<String, ServiceUsage> = HashMap::new();
    let mut live = std::collections::HashSet::new();
    for c in list {
        let (Some(id), Some(labels)) = (c.id, c.labels) else { continue };
        let Some(service) = labels.get(runtime::L_SERVICE) else { continue };
        live.insert(id.clone());
        let Some(cg) = cgroup_dir(&id) else { continue };
        let cpu_usec = read_kv(&cg.join("cpu.stat"), "usage_usec").unwrap_or(0);
        let mut memory = std::fs::read_to_string(cg.join("memory.current")).ok().and_then(|s| s.trim().parse::<u64>().ok());
        let mut net = net_bytes(&cg);
        if memory.is_none() || net.is_none() {
            // Nested or restricted cgroups: ask Docker for a one-shot sample instead.
            let opts = bollard::query_parameters::StatsOptions { stream: false, one_shot: true };
            if let Some(Ok(st)) = futures_util::StreamExt::next(&mut app.docker.stats(&id, Some(opts))).await {
                memory = memory.or(st.memory_stats.and_then(|m| m.usage));
                net = net.or_else(|| {
                    st.networks.map(|n| n.values().fold((0, 0), |(r, t), v| (r + v.rx_bytes.unwrap_or(0), t + v.tx_bytes.unwrap_or(0))))
                });
            }
        }
        let memory = memory.unwrap_or(0);
        let (rx, tx) = net.unwrap_or((0, 0));
        let now = std::time::Instant::now();
        let e = per.entry(service.clone()).or_insert_with(|| ServiceUsage { id: service.clone(), ..Default::default() });
        e.memory += memory;
        if let Some(p) = prev.get(&id) {
            let secs = now.duration_since(p.at).as_secs_f64().max(0.001);
            e.cpu += cpu_usec.saturating_sub(p.cpu_usec) as f64 / (secs * 1_000_000.0) * 100.0;
            e.net_rx += rx.saturating_sub(p.rx) as f64 / secs;
            e.net_tx += tx.saturating_sub(p.tx) as f64 / secs;
        }
        prev.insert(id, ContainerPrev { cpu_usec, rx, tx, at: now });
    }
    prev.retain(|id, _| live.contains(id));
    per.into_values().collect()
}

fn cgroup_dir(id: &str) -> Option<PathBuf> {
    let candidates = [
        format!("/sys/fs/cgroup/system.slice/docker-{id}.scope"),
        format!("/sys/fs/cgroup/docker/{id}"),
        format!("/sys/fs/cgroup/docker.slice/docker-{id}.scope"),
    ];
    candidates.into_iter().map(PathBuf::from).find(|p| p.join("cpu.stat").exists())
}

fn read_kv(path: &std::path::Path, key: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.lines().find_map(|l| l.strip_prefix(key)?.trim().parse().ok())
}

/// Network counters of the container's first process.
fn net_bytes(cg: &std::path::Path) -> Option<(u64, u64)> {
    let pid = std::fs::read_to_string(cg.join("cgroup.procs")).ok().and_then(|s| s.lines().next().map(String::from))?;
    let text = std::fs::read_to_string(format!("/proc/{pid}/net/dev")).ok()?;
    let mut rx = 0;
    let mut tx = 0;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
        if f.len() >= 9 {
            rx += f[0];
            tx += f[8];
        }
    }
    Some((rx, tx))
}

fn series_id(app: &App, conn: &rusqlite::Connection, node: &str, scope: &str, name: &str) -> Result<i64> {
    let key = (node.to_string(), scope.to_string(), name.to_string());
    if let Some(id) = crate::util::lock(&app.metrics.series).get(&key) {
        return Ok(*id);
    }
    conn.execute("INSERT OR IGNORE INTO metric_series (node_id, scope, name) VALUES (?1, ?2, ?3)", params![node, scope, name])?;
    let id: i64 = conn.query_row("SELECT id FROM metric_series WHERE node_id = ?1 AND scope = ?2 AND name = ?3", params![node, scope, name], |r| r.get(0))?;
    crate::util::lock(&app.metrics.series).insert(key, id);
    Ok(id)
}

/// Store a server's snapshot ("local" = the panel's own server).
pub fn store_for(app: &App, node: &str, s: &Snapshot) -> Result<()> {
    let mut points: Vec<(String, String, f64)> = vec![
        ("host".into(), "cpu".into(), s.cpu),
        ("host".into(), "memory".into(), s.memory.used as f64),
        ("host".into(), "swap".into(), s.swap.used as f64),
        ("host".into(), "load1".into(), s.load[0]),
        ("host".into(), "disk_read".into(), s.disk_io.rx),
        ("host".into(), "disk_write".into(), s.disk_io.tx),
        ("host".into(), "net_rx".into(), s.net.rx),
        ("host".into(), "net_tx".into(), s.net.tx),
    ];
    if let Some(t) = s.temp {
        points.push(("host".into(), "temp".into(), t));
    }
    for d in &s.disks {
        points.push(("host".into(), format!("disk:{}", d.mount), d.used as f64 / d.total.max(1) as f64 * 100.0));
    }
    for g in &s.gpus {
        let scope = format!("gpu:{}", g.index);
        points.push((scope.clone(), "util".into(), g.util));
        points.push((scope.clone(), "memory".into(), g.mem_used as f64));
        if let Some(t) = g.temp {
            points.push((scope.clone(), "temp".into(), t));
        }
        if let Some(p) = g.power {
            points.push((scope, "power".into(), p));
        }
    }
    for sv in &s.services {
        let scope = format!("service:{}", sv.id);
        points.push((scope.clone(), "cpu".into(), sv.cpu));
        points.push((scope.clone(), "memory".into(), sv.memory as f64));
        points.push((scope.clone(), "net_rx".into(), sv.net_rx));
        points.push((scope.clone(), "net_tx".into(), sv.net_tx));
        points.push((scope, "restarts".into(), sv.restarts as f64));
    }
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    for (scope, name, v) in points {
        let id = series_id(app, &tx, node, &scope, &name)?;
        tx.execute("INSERT OR REPLACE INTO metrics_raw (series, ts, v) VALUES (?1, ?2, ?3)", params![id, s.at, v])?;
    }
    tx.commit()?;
    Ok(())
}

/// Fold raw samples into 5-minute buckets and drop expired data.
fn rollup(app: &App) -> Result<()> {
    let now = util::now();
    let conn = app.db.conn();
    let last: i64 = conn.query_row("SELECT COALESCE(MAX(ts), 0) FROM metrics_5m", [], |r| r.get(0))?;
    let upto = now / 300 * 300;
    conn.execute(
        "INSERT OR REPLACE INTO metrics_5m (series, ts, v, vmax)
         SELECT series, ts / 300 * 300, AVG(v), MAX(v) FROM metrics_raw WHERE ts >= ?1 AND ts < ?2 GROUP BY series, ts / 300",
        params![last, upto],
    )?;
    conn.execute("DELETE FROM metrics_raw WHERE ts < ?1", [now - 86400])?;
    conn.execute("DELETE FROM metrics_5m WHERE ts < ?1", [now - 30 * 86400])?;
    Ok(())
}

/// Average of a series over the last `secs` seconds, added up over every server.
pub fn recent(app: &App, scope: &str, name: &str, secs: i64) -> Option<f64> {
    let conn = app.db.conn();
    conn.query_row(
        "SELECT SUM(a) FROM (SELECT AVG(r.v) AS a FROM metrics_raw r JOIN metric_series s ON s.id = r.series
           WHERE s.scope = ?1 AND s.name = ?2 AND r.ts >= ?3 GROUP BY r.series)",
        params![scope, name, util::now() - secs],
        |r| r.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
}

/// Average CPU of the online servers over the last `secs` seconds.
pub fn cluster_cpu(app: &App, secs: i64) -> Option<f64> {
    let conn = app.db.conn();
    conn.query_row(
        "SELECT AVG(a) FROM (SELECT AVG(r.v) AS a FROM metrics_raw r JOIN metric_series s ON s.id = r.series
           JOIN nodes n ON n.id = s.node_id
           WHERE s.scope = 'host' AND s.name = 'cpu' AND n.status = 'online' AND r.ts >= ?1 GROUP BY r.series)",
        params![util::now() - secs],
        |r| r.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
}

/// Points for a chart, downsampled to roughly 300 per series. Server scopes read `node`;
/// service scopes add up the service's usage on every server.
#[allow(clippy::type_complexity)]
pub fn query(app: &App, node: &str, scope: &str, names: &[String], range_s: i64) -> Result<(i64, HashMap<String, Vec<(i64, f64)>>)> {
    let now = util::now();
    let from = now - range_s;
    let step = (range_s / 300).max(STEP);
    let (table, step) = if range_s <= 86400 { ("metrics_raw", step) } else { ("metrics_5m", step.max(300)) };
    let conn = app.db.conn();
    let mut out = HashMap::new();
    let wanted: Vec<String> = if names.is_empty() {
        let mut stmt = conn.prepare("SELECT DISTINCT name FROM metric_series WHERE scope = ?1")?;
        stmt.query_map([scope], |r| r.get(0))?.filter_map(|r| r.ok()).collect()
    } else {
        names.to_vec()
    };
    let every_node = scope.starts_with("service:");
    for name in wanted {
        let ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM metric_series WHERE scope = ?1 AND name = ?2 AND (?3 OR node_id = ?4)")?;
            stmt.query_map(params![scope, name, every_node, node], |r| r.get(0))?.filter_map(|r| r.ok()).collect()
        };
        if ids.is_empty() {
            continue;
        }
        let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT b, SUM(v) FROM (SELECT series, ts / ?2 * ?2 AS b, AVG(v) AS v FROM {table} WHERE series IN ({list}) AND ts >= ?1 GROUP BY series, b) GROUP BY b ORDER BY b"
        ))?;
        let points: Vec<(i64, f64)> = stmt.query_map(params![from, step], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
        out.insert(name, points);
    }
    Ok((step, out))
}

pub fn parse_range(r: &str) -> i64 {
    match r {
        "15m" => 900,
        "1h" => 3600,
        "6h" => 21600,
        "24h" | "1d" => 86400,
        "7d" => 7 * 86400,
        "30d" => 30 * 86400,
        _ => 3600,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("7d"), 604800);
        assert_eq!(parse_range("junk"), 3600);
    }
}
