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

const STEP: i64 = 10;

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
    /// metric_series ids by "node\0scope\0name".
    series: Mutex<HashMap<String, i64>>,
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
            if !real_disk(&d.file_system().to_string_lossy(), d.total_space(), d.mount_point()) || !seen.insert(d.name()) {
                continue;
            }
            let u = d.usage();
            snap.disk_io.rx += u.read_bytes as f64 / STEP as f64;
            snap.disk_io.tx += u.written_bytes as f64 / STEP as f64;
            let mount = d.mount_point().to_string_lossy().into_owned();
            snap.disks.push(DiskUsage { mount, used: d.total_space().saturating_sub(d.available_space()), total: d.total_space() });
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
            *util::lock(&app.metrics.latest) = snap;
            continue;
        }
        let mut crashes = std::mem::take(&mut *util::lock(&app.metrics.restarts));
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
        *util::lock(&app.metrics.latest) = snap;
        if at - last_rollup >= 300 {
            if let Err(e) = rollup(&app) {
                tracing::warn!("metrics rollup: {e:#}");
            }
            last_rollup = at;
        }
    }
}

async fn gpus() -> Vec<Gpu> {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw", "--format=csv,noheader,nounits"]);
    if let Ok(csv) = crate::proc::output(cmd).await {
        return csv.lines().filter_map(nvidia_gpu).collect();
    }
    let mut out = vec![];
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

/// One line of nvidia-smi's CSV (memory in MiB).
fn nvidia_gpu(line: &str) -> Option<Gpu> {
    let mut f = line.split(',').map(str::trim);
    let mut next = || f.next();
    let (index, name, util, used, total, temp, power) = (next()?, next()?, next()?, next()?, next()?, next()?, next()?);
    let num = |s: &str| s.parse::<f64>().ok();
    Some(Gpu {
        index: index.parse().unwrap_or(0),
        name: name.to_string(),
        util: num(util).unwrap_or(0.0),
        mem_used: (num(used).unwrap_or(0.0) * 1048576.0) as u64,
        mem_total: (num(total).unwrap_or(0.0) * 1048576.0) as u64,
        temp: num(temp),
        power: num(power),
    })
}

/// Per-service usage from cgroup v2 files (cheap), summed over replicas.
async fn containers(app: &App, prev: &mut HashMap<String, ContainerPrev>) -> Vec<ServiceUsage> {
    let filters = runtime::managed();
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

/// A mount worth a disk card. Containers bind-mount single files such as /etc/resolv.conf from the
/// host disk, and those come first in the mount table, so they must lose to a directory mount.
fn real_disk(fs: &str, total: u64, mount: &std::path::Path) -> bool {
    total > 0 && !fs.contains("overlay") && !fs.contains("tmpfs") && mount.is_dir()
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
    let procs = std::fs::read_to_string(cg.join("cgroup.procs")).ok()?;
    let pid = procs.lines().next()?;
    Some(net_dev(&std::fs::read_to_string(format!("/proc/{pid}/net/dev")).ok()?))
}

/// Received and sent bytes over every interface but loopback in a /proc/net/dev table.
fn net_dev(text: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        if name.trim() == "lo" {
            continue;
        }
        let mut f = rest.split_whitespace().map(|x| x.parse::<u64>().ok());
        if let (Some(Some(r)), Some(Some(t))) = (f.next(), f.nth(7)) {
            rx = rx.saturating_add(r);
            tx = tx.saturating_add(t);
        }
    }
    (rx, tx)
}

pub fn percent(used: u64, total: u64) -> f64 {
    used as f64 / total.max(1) as f64 * 100.0
}

/// Store a server's snapshot ("local" = the panel's own server).
pub fn store_for(app: &App, node: &str, s: &Snapshot) -> Result<()> {
    let mut conn = app.db.conn();
    let tx = conn.transaction()?;
    let mut series = util::lock(&app.metrics.series);
    let mut key = String::new();
    let mut put = |scope: &str, name: &str, v: f64| -> Result<()> {
        key.clear();
        key.extend([node, "\0", scope, "\0", name]);
        let id = match series.get(&key) {
            Some(id) => *id,
            None => {
                tx.prepare_cached("INSERT OR IGNORE INTO metric_series (node_id, scope, name) VALUES (?1, ?2, ?3)")?.execute(params![node, scope, name])?;
                let id: i64 = tx.prepare_cached("SELECT id FROM metric_series WHERE node_id = ?1 AND scope = ?2 AND name = ?3")?.query_row(params![node, scope, name], |r| r.get(0))?;
                series.insert(key.clone(), id);
                id
            }
        };
        tx.prepare_cached("INSERT OR REPLACE INTO metrics_raw (series, ts, v) VALUES (?1, ?2, ?3)")?.execute(params![id, s.at, v])?;
        Ok(())
    };
    for (name, v) in [
        ("cpu", s.cpu),
        ("memory", s.memory.used as f64),
        ("swap", s.swap.used as f64),
        ("load1", s.load[0]),
        ("disk_read", s.disk_io.rx),
        ("disk_write", s.disk_io.tx),
        ("net_rx", s.net.rx),
        ("net_tx", s.net.tx),
    ] {
        put("host", name, v)?;
    }
    if let Some(t) = s.temp {
        put("host", "temp", t)?;
    }
    for d in &s.disks {
        put("host", &format!("disk:{}", d.mount), percent(d.used, d.total))?;
    }
    for g in &s.gpus {
        let scope = format!("gpu:{}", g.index);
        put(&scope, "util", g.util)?;
        put(&scope, "memory", g.mem_used as f64)?;
        if let Some(t) = g.temp {
            put(&scope, "temp", t)?;
        }
        if let Some(p) = g.power {
            put(&scope, "power", p)?;
        }
    }
    for sv in &s.services {
        let scope = format!("service:{}", sv.id);
        for (name, v) in [("cpu", sv.cpu), ("memory", sv.memory as f64), ("net_rx", sv.net_rx), ("net_tx", sv.net_tx), ("restarts", sv.restarts as f64)] {
            put(&scope, name, v)?;
        }
    }
    drop(series);
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
    app.db.row(
        "SELECT SUM(a) FROM (SELECT AVG(r.v) AS a FROM metrics_raw r JOIN metric_series s ON s.id = r.series
           WHERE s.scope = ?1 AND s.name = ?2 AND r.ts >= ?3 GROUP BY r.series)",
        params![scope, name, util::now() - secs],
        |r| r.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
    .flatten()
}

/// Average CPU of the online servers over the last `secs` seconds.
pub fn cluster_cpu(app: &App, secs: i64) -> Option<f64> {
    app.db.row(
        "SELECT AVG(a) FROM (SELECT AVG(r.v) AS a FROM metrics_raw r JOIN metric_series s ON s.id = r.series
           JOIN nodes n ON n.id = s.node_id
           WHERE s.scope = 'host' AND s.name = 'cpu' AND n.status = 'online' AND r.ts >= ?1 GROUP BY r.series)",
        params![util::now() - secs],
        |r| r.get::<_, Option<f64>>(0),
    )
    .ok()
    .flatten()
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
    let mut out = HashMap::new();
    let wanted: Vec<String> = if names.is_empty() { app.db.rows("SELECT DISTINCT name FROM metric_series WHERE scope = ?1", [scope], |r| r.get(0))? } else { names.to_vec() };
    let every_node = scope.starts_with("service:");
    let series = "SELECT id FROM metric_series WHERE scope = ?1 AND name = ?2 AND (?3 OR node_id = ?4)";
    let points = format!(
        "SELECT b, SUM(v) FROM (SELECT series, ts / ?6 * ?6 AS b, AVG(v) AS v FROM {table} WHERE series IN ({series}) AND ts >= ?5 GROUP BY series, b) GROUP BY b ORDER BY b"
    );
    for name in wanted {
        if app.db.row(series, params![scope, name, every_node, node], |_| Ok(()))?.is_none() {
            continue;
        }
        let p = app.db.rows(&points, params![scope, name, every_node, node, from, step], |r| Ok((r.get(0)?, r.get(1)?)))?;
        out.insert(name, p);
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

    #[test]
    fn parses_gpus_and_net_dev() {
        let g = nvidia_gpu("0, NVIDIA GeForce RTX 4070 Ti, 37, 1024, 12282, 51, 64.20").unwrap();
        assert_eq!((g.index, g.name.as_str(), g.util, g.mem_used, g.temp, g.power), (0, "NVIDIA GeForce RTX 4070 Ti", 37.0, 1 << 30, Some(51.0), Some(64.2)));
        assert_eq!(nvidia_gpu("0, GPU, [N/A], 1, 2, [N/A], [N/A]").map(|g| (g.util, g.temp)), Some((0.0, None)));
        assert!(nvidia_gpu("0, short").is_none());
        let dev = "Inter-|   Receive\n face |bytes packets\n    lo: 500 1 0 0 0 0 0 0 500 1 0 0 0 0 0 0\n  eth0: 100 2 0 0 0 0 0 0 40 3 0 0 0 0 0 0\n  eth1: 5 0 0 0 0 0 0 0 6 0 0 0 0 0 0 0\n";
        assert_eq!(net_dev(dev), (105, 46));
    }

    #[test]
    fn file_bind_mounts_are_not_disks() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(real_disk("ext4", 1, root));
        assert!(!real_disk("ext4", 1, &root.join("Cargo.toml")));
        assert!(!real_disk("ext4", 1, &root.join("no-such-mount")));
        assert!(!real_disk("overlay", 1, root));
        assert!(!real_disk("tmpfs", 1, root));
        assert!(!real_disk("ext4", 0, root));
    }
}
