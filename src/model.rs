//! What a service is. Stored as JSON in `services.spec`; every field has a default so
//! old specs keep loading as the format grows.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ServiceSpec {
    pub source: Source,
    pub build: Build,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub replicas: u32,
    pub ports: Vec<Port>,
    pub mounts: Vec<Mount>,
    pub resources: Resources,
    pub health: Health,
    pub rollout: Rollout,
    /// None = use the instance default (cancel the running deploy).
    pub deploy_policy: Option<DeployPolicy>,
    pub gpu: bool,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub groups: Vec<String>,
    pub sysctls: BTreeMap<String, String>,
    /// Dangerous switches: only admins with the `destroy` grant or the owner may turn them on.
    pub privileged: bool,
    pub docker_socket: bool,
    pub host_network: bool,
    /// Blocks setuid escalation inside the container. On by default for new services.
    pub no_new_privileges: bool,
    pub auto_update: AutoUpdate,
    pub placement: Placement,
    pub autoscale: Option<Autoscale>,
    pub sleep: Option<Sleep>,
    pub database: Option<DatabaseSpec>,
    pub compose: Option<ComposeSpec>,
    /// Env var names passed to builds as build args (in addition to ARGs the Dockerfile declares).
    pub build_args: Vec<String>,
    /// Extra DNS names on the project network (migrated services keep their old hostnames).
    pub aliases: Vec<String>,
    /// Extra Docker networks to join, by name.
    pub networks: Vec<String>,
}

impl Default for ServiceSpec {
    fn default() -> Self {
        ServiceSpec {
            source: Source::default(),
            build: Build::default(),
            command: None,
            entrypoint: None,
            working_dir: None,
            user: None,
            replicas: 1,
            ports: vec![],
            mounts: vec![],
            resources: Resources::default(),
            health: Health::default(),
            rollout: Rollout::default(),
            deploy_policy: None,
            gpu: false,
            cap_add: vec![],
            cap_drop: vec![],
            groups: vec![],
            sysctls: BTreeMap::new(),
            privileged: false,
            docker_socket: false,
            host_network: false,
            no_new_privileges: true,
            auto_update: AutoUpdate::default(),
            placement: Placement::default(),
            autoscale: None,
            sleep: None,
            database: None,
            compose: None,
            build_args: vec![],
            aliases: vec![],
            networks: vec![],
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Git {
        /// Git provider id (GitHub App, Forgejo...). None = public repository or deploy key.
        #[serde(default)]
        provider: Option<String>,
        /// `owner/name` for providers, full URL otherwise.
        repo: String,
        #[serde(default = "default_branch")]
        branch: String,
        /// Build context inside the repository.
        #[serde(default)]
        path: String,
        /// Redeploy only when a push touches these paths (globs). Empty = any change.
        #[serde(default)]
        watch_paths: Vec<String>,
        #[serde(default = "yes")]
        auto_deploy: bool,
    },
    Image {
        image: String,
    },
    Dockerfile {
        content: String,
    },
    Compose {
        yaml: String,
    },
}

impl Default for Source {
    fn default() -> Self {
        Source::Image { image: String::new() }
    }
}

fn default_branch() -> String {
    "main".into()
}
fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Build {
    /// Dockerfile if the repository has one, otherwise Railpack.
    #[default]
    Auto,
    Dockerfile {
        #[serde(default = "default_dockerfile")]
        path: String,
        #[serde(default)]
        target: Option<String>,
    },
    Railpack,
    Nixpacks,
    Buildpacks {
        #[serde(default = "default_builder")]
        builder: String,
    },
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}
fn default_builder() -> String {
    "heroku/builder:24".into()
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Port {
    pub container: u16,
    pub host: u16,
    #[serde(default = "tcp")]
    pub protocol: Protocol,
}

fn tcp() -> Protocol {
    Protocol::Tcp
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Mount {
    Volume {
        name: String,
        target: String,
        #[serde(default)]
        readonly: bool,
    },
    Bind {
        source: String,
        target: String,
        #[serde(default)]
        readonly: bool,
    },
    /// Contents live encrypted in `service_files`.
    File {
        target: String,
    },
}

impl Mount {
    pub fn target(&self) -> &str {
        match self {
            Mount::Volume { target, .. } | Mount::Bind { target, .. } | Mount::File { target } => target,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Resources {
    pub cpus: Option<f64>,
    pub memory_mb: Option<u64>,
    pub memory_reservation_mb: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthKind {
    /// Image HEALTHCHECK if it has one, else HTTP on the first domain's port, else "running".
    #[default]
    Auto,
    Http,
    Tcp,
    Command,
    None,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Health {
    pub kind: HealthKind,
    pub path: String,
    pub port: Option<u16>,
    pub command: Option<String>,
    pub interval_s: u32,
    pub timeout_s: u32,
    pub retries: u32,
    pub start_period_s: u32,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            kind: HealthKind::Auto,
            path: "/".into(),
            port: None,
            command: None,
            interval_s: 5,
            timeout_s: 5,
            retries: 3,
            start_period_s: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Start the new version, switch traffic once it is healthy, then stop the old one.
    #[default]
    StartFirst,
    /// Stop the old version first (for workers that must never run twice, like bots).
    StopFirst,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Rollout {
    pub strategy: Strategy,
    /// Seconds old replicas keep serving in-flight requests after traffic moves.
    pub drain_s: u32,
    /// How long a new version may take to become healthy before the deploy fails.
    pub health_timeout_s: u32,
    /// Images kept for one-click rollback.
    pub keep_images: u32,
}

impl Default for Rollout {
    fn default() -> Self {
        Rollout { strategy: Strategy::StartFirst, drain_s: 10, health_timeout_s: 180, keep_images: 3 }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeployPolicy {
    /// A new deploy cancels the one that is running.
    #[default]
    Cancel,
    /// A new deploy waits for the running one to finish.
    Queue,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    /// Redeploy when the tag points to a new image digest.
    #[default]
    Digest,
    /// Move to the newest version with the same major number.
    SameMajor,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AutoUpdate {
    pub enabled: bool,
    pub interval_minutes: u32,
    pub policy: UpdatePolicy,
}

impl Default for AutoUpdate {
    fn default() -> Self {
        AutoUpdate { enabled: false, interval_minutes: 360, policy: UpdatePolicy::Digest }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Placement {
    pub node: Option<String>,
    pub region: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Autoscale {
    pub min: u32,
    pub max: u32,
    /// Target average CPU per replica, in percent of one core.
    pub cpu_percent: Option<u32>,
    /// Target requests per second per replica.
    pub rps: Option<u32>,
}

impl Default for Autoscale {
    fn default() -> Self {
        Autoscale { min: 1, max: 3, cpu_percent: Some(70), rps: None }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Sleep {
    /// Stop after this many minutes without requests; wake on the next one.
    pub idle_minutes: u32,
}

impl Default for Sleep {
    fn default() -> Self {
        Sleep { idle_minutes: 30 }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
    Mariadb,
    Mongo,
    Redis,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DatabaseSpec {
    pub engine: Engine,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub database: String,
    /// Read replicas (milestone 3): node ids or regions they run in.
    #[serde(default)]
    pub replicas: Vec<ReplicaSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReplicaSpec {
    pub id: String,
    #[serde(default)]
    pub placement: Placement,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ComposeSpec {
    /// Compose file inside the repository (Git source).
    pub file: String,
}

impl Default for ComposeSpec {
    fn default() -> Self {
        ComposeSpec { file: "docker-compose.yml".into() }
    }
}

/// Per-domain routing options (JSON in `domains.options`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct DomainOptions {
    pub www_redirect: Option<WwwRedirect>,
    pub redirects: Vec<Redirect>,
    pub basic_auth: Vec<BasicUser>,
    pub forward_auth: Option<ForwardAuth>,
    pub strip_prefix: bool,
    pub rewrite: Option<Rewrite>,
    /// When set, every request gets this page with status 503.
    pub maintenance: Option<String>,
    /// Custom pages per upstream error status (502, 503, 504).
    pub error_pages: BTreeMap<u16, String>,
    pub timeout_s: Option<u64>,
    pub max_body_mb: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WwwRedirect {
    ToWww,
    ToApex,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Redirect {
    /// Regex matched against the full URL.
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub permanent: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BasicUser {
    pub user: String,
    /// Argon2 hash of the password. Never shown to people or agents.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hash: String,
    /// A new password: hashed when saved, never stored. Leave it out to keep the current one.
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
}

/// Domain options as shown to people and agents: without password hashes.
pub fn public_options<S: serde::Serializer>(o: &DomainOptions, s: S) -> Result<S::Ok, S::Error> {
    let mut o = o.clone();
    for u in &mut o.basic_auth {
        u.hash.clear();
    }
    o.serialize(s)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ForwardAuth {
    pub url: String,
    /// Headers copied from the auth response to the upstream request.
    #[serde(default)]
    pub response_headers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rewrite {
    pub from: String,
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_specs_keep_loading() {
        let spec: ServiceSpec = serde_json::from_str(r#"{"source":{"type":"image","image":"nginx:1.29"}}"#).unwrap();
        assert_eq!(spec.replicas, 1);
        assert_eq!(spec.rollout.strategy, Strategy::StartFirst);
        assert_eq!(spec.build, Build::Auto);
        let git: Source = serde_json::from_str(r#"{"type":"git","repo":"o/r"}"#).unwrap();
        assert!(matches!(git, Source::Git { ref branch, auto_deploy: true, .. } if branch == "main"));
    }
}
