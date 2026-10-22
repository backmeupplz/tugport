use crate::auth::Principal;
use crate::crypto::Keyring;
use crate::db::Db;
use crate::model::DeployPolicy;
use crate::paths::Paths;
use crate::util;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

pub type AppState = Arc<App>;

pub struct App {
    pub paths: Paths,
    pub db: Db,
    pub keys: Keyring,
    pub docker: bollard::Docker,
    pub events: broadcast::Sender<Event>,
    pub deployer: crate::deploy::Deployer,
    pub build_slots: Arc<tokio::sync::Semaphore>,
    pub proxy: crate::proxy::Proxy,
    pub logs: broadcast::Sender<crate::logs::LogLine>,
    pub selflog: Arc<crate::logs::SelfLog>,
    pub metrics: crate::metrics::Metrics,
}

/// Something that happened, streamed to the UI and API clients that may see it.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    pub data: serde_json::Value,
    pub at: i64,
}

impl App {
    pub fn emit(&self, kind: &str, project_id: Option<&str>, service_id: Option<&str>, data: serde_json::Value) {
        let _ = self.events.send(Event {
            kind: kind.into(),
            project_id: project_id.map(Into::into),
            service_id: service_id.map(Into::into),
            data,
            at: util::now_ms(),
        });
    }

    /// Record who did what. Never pass secret values in `detail`.
    pub fn audit(
        &self,
        who: Option<&Principal>,
        action: &str,
        project_id: Option<&str>,
        service_id: Option<&str>,
        target: Option<&str>,
        detail: Option<&str>,
    ) {
        let res = self.db.conn().execute(
            "INSERT INTO audit (at, actor_id, actor_name, actor_kind, key_id, ip, action, project_id, service_id, target, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                util::now(),
                who.map(|p| p.user_id.clone()),
                who.map(|p| p.name.clone()).unwrap_or_else(|| "tugport".into()),
                who.map(|p| if p.kind == crate::auth::UserKind::Agent { "agent" } else { "human" }).unwrap_or("system"),
                who.and_then(|p| p.key_id.clone()),
                who.and_then(|p| p.ip.clone()),
                action,
                project_id,
                service_id,
                target,
                detail,
            ],
        );
        if let Err(e) = res {
            tracing::error!("writing audit log: {e}");
        }
        self.emit(
            "audit",
            project_id,
            service_id,
            serde_json::json!({ "action": action, "actor": who.map(|p| p.name.as_str()).unwrap_or("tugport"), "target": target }),
        );
    }

    pub fn settings(&self) -> Settings {
        self.db.get_setting("instance").ok().flatten().unwrap_or_default()
    }
}

/// Instance-wide settings, stored as one JSON document.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Domain the panel is served on through the proxy (with HTTPS).
    pub panel_domain: Option<String>,
    /// Address the raw panel port listens on. Empty disables it once a panel domain works.
    pub panel_listen: String,
    /// Extra address serving the API (e.g. `0.0.0.0:3000` so Easypanel's webhook URLs keep working).
    pub legacy_listen: Option<String>,
    pub acme_email: Option<String>,
    pub deploy_policy: DeployPolicy,
    pub docker_cleanup: DockerCleanup,
    pub log_cleanup: LogCleanup,
    /// Proxies whose X-Forwarded-* headers are trusted (Cloudflare, load balancers).
    pub trusted_proxies: Vec<ipnet::IpNet>,
    pub builds: BuildSettings,
    pub alerts: AlertSettings,
    pub updates: UpdateSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            panel_domain: None,
            panel_listen: "0.0.0.0:8484".into(),
            legacy_listen: None,
            acme_email: None,
            deploy_policy: DeployPolicy::Cancel,
            docker_cleanup: DockerCleanup::default(),
            log_cleanup: LogCleanup::default(),
            trusted_proxies: vec![],
            builds: BuildSettings::default(),
            alerts: AlertSettings::default(),
            updates: UpdateSettings::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DockerCleanup {
    pub enabled: bool,
    pub interval_hours: u32,
    /// Build cache kept, in GB.
    pub build_cache_gb: u32,
}

impl Default for DockerCleanup {
    fn default() -> Self {
        DockerCleanup { enabled: true, interval_hours: 6, build_cache_gb: 20 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LogCleanup {
    pub enabled: bool,
    pub max_gb: f64,
    pub max_days: Option<u32>,
}

impl Default for LogCleanup {
    fn default() -> Self {
        LogCleanup { enabled: false, max_gb: 10.0, max_days: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct BuildSettings {
    pub max_parallel: u32,
    /// Cap for all build steps together, in CPU cores. None = half of the machine.
    pub cpus: Option<f64>,
    /// Cap for all build steps together, in MB. None = half of the memory.
    pub memory_mb: Option<u64>,
}

impl Default for BuildSettings {
    fn default() -> Self {
        BuildSettings { max_parallel: 2, cpus: None, memory_mb: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AlertSettings {
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub disk_percent: f64,
    pub gpu_temp_c: f64,
    /// Minutes a condition must last before alerting.
    pub for_minutes: u32,
}

impl Default for AlertSettings {
    fn default() -> Self {
        AlertSettings { cpu_percent: 90.0, memory_percent: 90.0, disk_percent: 90.0, gpu_temp_c: 85.0, for_minutes: 5 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct UpdateSettings {
    /// Install new Tugport releases automatically.
    pub auto: bool,
    pub channel: String,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings { auto: false, channel: "stable".into() }
    }
}
