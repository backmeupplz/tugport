//! Turning services into running containers: networks, container configs, health checks.

use crate::app::{App, AppState};
use crate::model::{Health, HealthKind, Mount, Protocol, ServiceSpec};
use crate::nodes::NodeHandle;
use crate::secrets;
use crate::store::{self, Project, Service};
use anyhow::{Context, Result, anyhow, bail};
use bollard::models::{
    ContainerCreateBody, DeviceRequest, EndpointSettings, HealthConfig, HealthStatusEnum, HostConfig, HostConfigLogConfig,
    Mount as DockerMount, MountTypeEnum, NetworkCreateRequest, NetworkingConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, ListContainersOptions, RemoveContainerOptions, RemoveVolumeOptions, StopContainerOptions,
};
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

pub const L_MANAGED: &str = "tugport.managed";
pub const L_PROJECT: &str = "tugport.project";
pub const L_SERVICE: &str = "tugport.service";
pub const L_DEPLOY: &str = "tugport.deploy";
const L_REPLICA: &str = "tugport.replica";
pub const L_NODE: &str = "tugport.node";

/// With one server, projects get a bridge network; with more, an overlay spanning them.
pub fn network_name(project: &str) -> String {
    format!("tp-{project}")
}

pub fn overlay_name(project: &str) -> String {
    format!("tpm-{project}")
}

/// Bridge on every server of a multi-server setup that its proxy reaches replicas through;
/// containers on it cannot talk to each other.
const INGRESS: &str = "tp-ingress";

#[derive(Serialize, Clone, Debug)]
pub struct Replica {
    pub id: String,
    pub name: String,
    pub deploy: String,
    pub replica: u32,
    pub state: String,
    pub health: Option<String>,
    pub ip: Option<String>,
    pub image: String,
    pub created: i64,
    pub node: String,
    /// The server's name, for people who may not list servers.
    pub node_name: String,
    /// Compose service name for containers of Compose stacks.
    pub compose_service: Option<String>,
}

/// Docker's `filters` that match containers carrying all these labels.
pub fn label_filter(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
    HashMap::from([("label".into(), pairs.iter().map(|(k, v)| format!("{k}={v}")).collect())])
}

/// Docker's `filters` that match every container Tugport runs.
pub fn managed() -> HashMap<String, Vec<String>> {
    label_filter(&[(L_MANAGED, "true")])
}

/// Labels that tie a container to its service, deploy and server.
pub fn labels(s: &Service, deploy_id: &str, node: &str) -> [(&'static str, String); 5] {
    [(L_MANAGED, "true".into()), (L_PROJECT, s.project_id.clone()), (L_SERVICE, s.id.clone()), (L_DEPLOY, deploy_id.into()), (L_NODE, node.into())]
}

/// Log rotation of every container Tugport runs.
pub const LOG_LIMITS: [(&str, &str); 2] = [("max-size", "10m"), ("max-file", "3")];

/// A container's name as Docker lists it (`/web-1`), without the slash.
pub fn listed_name(names: Option<Vec<String>>) -> String {
    names.and_then(|n| n.into_iter().next()).unwrap_or_default().trim_start_matches('/').to_string()
}

/// (node id, Docker) of every online server; just this one before nodes are loaded.
pub fn dockers(app: &App) -> Vec<(String, bollard::Docker)> {
    let all: Vec<_> = app.all_nodes().into_iter().filter(|n| crate::nodes::online(app, &n.id)).map(|n| (n.id.clone(), n.docker.clone())).collect();
    if all.is_empty() { vec![("local".to_string(), app.docker.clone())] } else { all }
}

/// Containers that belong to a service on every reachable server, newest first.
pub async fn replicas(app: &App, service_id: &str) -> Result<Vec<Replica>> {
    Ok(list(app, &[(L_SERVICE, service_id)]).await?.into_iter().map(|(_, r)| r).collect())
}

/// Every service's containers, with one listing per server.
pub async fn all_replicas(app: &App) -> Result<HashMap<String, Vec<Replica>>> {
    let mut out: HashMap<String, Vec<Replica>> = HashMap::new();
    for (service, r) in list(app, &[(L_MANAGED, "true")]).await? {
        out.entry(service).or_default().push(r);
    }
    Ok(out)
}

/// Every service's containers, by service id.
pub type Listing = std::sync::Arc<HashMap<String, Vec<Replica>>>;

/// Every service's containers as the proxy last listed them (container events keep it current),
/// or a fresh listing before its first one.
pub async fn known_replicas(app: &App) -> Result<Listing> {
    if let Some(r) = crate::util::read(&app.proxy.replicas).clone() {
        return Ok(r);
    }
    Ok(std::sync::Arc::new(all_replicas(app).await?))
}

/// (service id, replica) of matching containers on every reachable server, newest first.
async fn list(app: &App, labels: &[(&str, &str)]) -> Result<Vec<(String, Replica)>> {
    let opts = ListContainersOptions { all: true, filters: Some(label_filter(labels)), ..Default::default() };
    let lists = futures_util::future::join_all(dockers(app).into_iter().map(|(node, docker)| {
        let opts = opts.clone();
        async move { (node, docker.list_containers(Some(opts)).await) }
    }))
    .await;
    let names: HashMap<String, String> = app.all_nodes().into_iter().map(|n| (n.id.clone(), n.name.clone())).collect();
    let mut out = vec![];
    for (node, list) in lists {
        let list = match list {
            Ok(l) => l,
            Err(e) if node == "local" => return Err(e.into()),
            Err(_) => continue,
        };
        for c in list {
            let mut labels = c.labels.unwrap_or_default();
            let Some(service) = labels.remove(L_SERVICE) else { continue };
            let nets = c.network_settings.and_then(|n| n.networks).unwrap_or_default();
            let ip_of = |e: &EndpointSettings| e.ip_address.clone().filter(|ip| !ip.is_empty());
            // The proxy on the replica's server reaches it on the ingress bridge, or on any
            // bridge it is on (overlay addresses are not reachable from the host), Tugport's
            // own first (during a migration a container also sits on the old panel's overlays).
            let mut bridges: Vec<_> = nets.iter().filter(|(k, _)| !k.starts_with("tpm-")).collect();
            bridges.sort_by_key(|(k, _)| !k.starts_with("tp-"));
            let ip = nets.get(INGRESS).and_then(ip_of).or_else(|| bridges.into_iter().find_map(|(_, e)| ip_of(e)));
            let health = c.status.as_deref().and_then(|s| {
                ["(healthy)", "(unhealthy)", "(health: starting)"].iter().find(|h| s.contains(*h)).map(|h| h.trim_matches(['(', ')']).replace("health: ", ""))
            });
            out.push((service, Replica {
                id: c.id.unwrap_or_default(),
                name: listed_name(c.names),
                deploy: labels.remove(L_DEPLOY).unwrap_or_default(),
                replica: labels.get(L_REPLICA).and_then(|r| r.parse().ok()).unwrap_or(0),
                state: c.state.map(|s| s.to_string()).unwrap_or_default(),
                health,
                ip,
                image: c.image.unwrap_or_default(),
                created: c.created.unwrap_or_default(),
                node: node.clone(),
                node_name: names.get(&node).cloned().unwrap_or_else(|| node.clone()),
                compose_service: labels.remove("com.docker.compose.service"),
            }));
        }
    }
    out.sort_by(|(_, a), (_, b)| b.created.cmp(&a.created).then(a.replica.cmp(&b.replica)));
    Ok(out)
}

/// The deploy currently receiving traffic.
pub fn live_deploy(app: &App, service_id: &str) -> Option<String> {
    app.db.state::<String>(&format!("live:{service_id}"))
}

/// Replicas of the live deploy (all of them before the first deploy, and every container of a
/// Compose stack): what serves the service right now.
pub async fn current(app: &App, s: &Service) -> Result<Vec<Replica>> {
    let live = live_deploy(app, &s.id);
    Ok(replicas(app, &s.id).await?.into_iter().filter(|r| s.kind == "compose" || live.as_deref().is_none_or(|l| r.deploy == l)).collect())
}

/// One running replica of the live deploy, for commands that need the service up.
pub async fn running_replica(app: &App, s: &Service) -> Result<Replica> {
    current(app, s).await?.into_iter().find(|r| r.state == "running").ok_or_else(|| anyhow::anyhow!("{} is not running", s.name))
}

/// The image the live deploy runs.
pub fn live_image(app: &App, service_id: &str) -> Option<String> {
    crate::deploy::get(app, &live_deploy(app, service_id)?).ok().flatten()?.image
}

pub fn set_live_deploy(app: &App, service_id: &str, deploy_id: &str) -> Result<()> {
    app.db.set_state(&format!("live:{service_id}"), deploy_id)
}

#[derive(Serialize)]
struct Status {
    pub state: &'static str,
    pub running: usize,
    pub desired: u32,
    pub live_deploy: Option<String>,
    pub replicas: Vec<Replica>,
}

pub async fn service_status(app: &App, s: &Service) -> serde_json::Value {
    status_of(app, s, replicas(app, &s.id).await.unwrap_or_default())
}

/// A service's state from its containers.
pub fn status_of(app: &App, s: &Service, reps: Vec<Replica>) -> serde_json::Value {
    let live = live_deploy(app, &s.id);
    let current: Vec<&Replica> = reps.iter().filter(|r| live.as_deref().is_none_or(|l| r.deploy == l)).collect();
    let running = current.iter().filter(|r| r.state == "running").count();
    let state = if s.kind == "compose" {
        if reps.iter().any(|r| r.state == "running") { "running" } else if reps.is_empty() { "not deployed" } else { "stopped" }
    } else if reps.is_empty() {
        "not deployed"
    } else if current.iter().any(|r| r.state == "restarting") {
        "crashing"
    } else if running == 0 {
        if crate::sleep::is_asleep(app, &s.id) { "asleep" } else { "stopped" }
    } else if current.iter().any(|r| r.health.as_deref() == Some("unhealthy")) {
        "unhealthy"
    } else if (running as u32) < s.spec.replicas {
        "degraded"
    } else {
        "running"
    };
    serde_json::to_value(Status { state, running, desired: s.spec.replicas, live_deploy: live, replicas: reps }).unwrap_or_default()
}

pub async fn ensure_network(app: &App, project: &str) -> Result<String> {
    let overlay = crate::nodes::mesh_enabled(app);
    let name = if overlay { overlay_name(project) } else { network_name(project) };
    let mut options = HashMap::new();
    if overlay {
        // WireGuard (1420) minus the VXLAN header.
        options.insert("com.docker.network.driver.mtu".to_string(), "1370".to_string());
        if crate::nodes::encrypt_overlay(app) {
            options.insert("encrypted".to_string(), String::new());
        }
    }
    let req = NetworkCreateRequest {
        name: name.clone(),
        driver: Some(if overlay { "overlay" } else { "bridge" }.into()),
        attachable: overlay.then_some(true),
        options: non_empty(options),
        ..Default::default()
    };
    create_network(&app.docker, req).await?;
    Ok(name)
}

/// Create a Tugport network unless it exists (or another deploy creates it meanwhile).
async fn create_network(docker: &bollard::Docker, req: NetworkCreateRequest) -> Result<()> {
    if docker.inspect_network(&req.name, None::<bollard::query_parameters::InspectNetworkOptions>).await.is_ok() {
        return Ok(());
    }
    let name = req.name.clone();
    let req = NetworkCreateRequest { labels: Some(HashMap::from([(L_MANAGED.to_string(), "true".to_string())])), ..req };
    match docker.create_network(req).await {
        Ok(_) | Err(bollard::errors::Error::DockerResponseServerError { status_code: 409, .. }) => Ok(()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("creating network {name}"))),
    }
}

/// Host interfaces of this server's Docker bridge networks.
pub async fn bridge_interfaces(docker: &bollard::Docker) -> Result<Vec<String>> {
    let nets = docker.list_networks(None::<bollard::query_parameters::ListNetworksOptions>).await.context("listing Docker networks")?;
    Ok(nets
        .into_iter()
        .filter(|n| n.driver.as_deref() == Some("bridge"))
        .filter_map(|n| match n.options.as_ref().and_then(|o| o.get("com.docker.network.bridge.name")) {
            Some(name) => Some(name.clone()),
            None => Some(format!("br-{}", n.id?.get(..12)?)),
        })
        .collect())
}

/// The ingress bridge on a server (multi-server setups only).
async fn ensure_ingress(docker: &bollard::Docker) -> Result<()> {
    let options = HashMap::from([("com.docker.network.bridge.enable_icc".to_string(), "false".to_string())]);
    create_network(docker, NetworkCreateRequest { name: INGRESS.into(), driver: Some("bridge".into()), options: Some(options), ..Default::default() }).await
}

/// First server added: running containers join their project's overlay with the same
/// names, so services on other servers reach them before they are redeployed.
pub async fn move_to_overlays(app: &App) -> Result<()> {
    for s in store::services(app, None)? {
        let network = ensure_network(app, &s.project).await?;
        for r in replicas(app, &s.id).await? {
            if r.node != "local" || r.state != "running" {
                continue;
            }
            let aliases = if s.kind == "compose" { r.compose_service.iter().map(|c| format!("{}-{c}", s.name)).collect() } else { aliases(&s) };
            let req = bollard::models::NetworkConnectRequest {
                container: Some(r.id.clone()),
                endpoint_config: Some(EndpointSettings { aliases: Some(aliases), ..Default::default() }),
            };
            if let Err(e) = app.docker.connect_network(&network, req).await {
                tracing::warn!("connecting {} to {network}: {e}", r.name);
            }
        }
    }
    Ok(())
}

pub async fn remove_project_network(app: &App, p: &Project) {
    let _ = app.docker.remove_network(&network_name(&p.name)).await;
    let _ = app.docker.remove_network(&overlay_name(&p.name)).await;
}

pub async fn pull(app: &App, image: &str, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    pull_on(app, &app.docker, image, log).await
}

pub async fn pull_on(app: &App, docker: &bollard::Docker, image: &str, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let (repo, tag) = split_image(image);
    let creds = registry_credentials(app, image)?;
    let mut stream = docker.create_image(
        Some(CreateImageOptions { from_image: Some(repo.to_string()), tag: Some(tag.to_string()), ..Default::default() }),
        None,
        creds,
    );
    let mut last = String::new();
    while let Some(item) = stream.next().await {
        let info = item.with_context(|| format!("pulling {image}"))?;
        if let Some(status) = info.status {
            let line = match info.id {
                Some(id) => format!("{id}: {status}"),
                None => status,
            };
            if line != last && !line.contains("Downloading") && !line.contains("Extracting") {
                log(&line);
                last = line;
            }
        }
    }
    Ok(())
}

/// `ghcr.io/o/r:1.2` -> (`ghcr.io/o/r`, `1.2`); digests and missing tags handled.
pub fn split_image(image: &str) -> (&str, &str) {
    if let Some((repo, digest)) = image.split_once('@') {
        return (repo, digest);
    }
    match image.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => (repo, tag),
        _ => (image, "latest"),
    }
}

pub fn registry_host(image: &str) -> &str {
    match image.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => first,
        _ => "docker.io",
    }
}

/// The saved login (user, password) for the registry `image` comes from, if any.
pub fn registry_login(app: &App, image: &str) -> Result<Option<(String, zeroize::Zeroizing<String>)>> {
    let host = registry_host(image);
    let row: Option<(String, Vec<u8>)> = app.db.row("SELECT username, password FROM registries WHERE host = ?1", [host], |r| Ok((r.get(0)?, r.get(1)?)))?;
    row.map(|(user, pw)| Ok((user, app.keys.open_str(&format!("registry:{host}"), &pw)?))).transpose()
}

fn registry_credentials(app: &App, image: &str) -> Result<Option<bollard::auth::DockerCredentials>> {
    Ok(registry_login(app, image)?.map(|(user, pw)| bollard::auth::DockerCredentials {
        username: Some(user),
        password: Some(pw.to_string()),
        serveraddress: Some(registry_host(image).to_string()),
        ..Default::default()
    }))
}

pub struct ImageInfo {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub has_healthcheck: bool,
    pub exposed: Vec<u16>,
}

pub async fn image_info(app: &App, image: &str) -> Result<ImageInfo> {
    let i = app.docker.inspect_image(image).await.with_context(|| format!("inspecting {image}"))?;
    let c = i.config.unwrap_or_default();
    let has_healthcheck = c.healthcheck.as_ref().and_then(|h| h.test.as_ref()).is_some_and(|t| t.first().is_some_and(|f| f != "NONE"));
    let exposed = c.exposed_ports.unwrap_or_default().keys().filter_map(|k| k.split('/').next()?.parse().ok()).collect();
    Ok(ImageInfo { entrypoint: c.entrypoint.unwrap_or_default(), cmd: c.cmd.unwrap_or_default(), has_healthcheck, exposed })
}

/// The process a container should run, before it is wrapped by Tugport's init.
fn final_command(spec: &ServiceSpec, img: &ImageInfo) -> Vec<String> {
    match (&spec.entrypoint, &spec.command) {
        (Some(ep), cmd) => ep.iter().chain(cmd.iter().flatten()).cloned().collect(),
        (None, Some(cmd)) => img.entrypoint.iter().chain(cmd.iter()).cloned().collect(),
        (None, None) => img.entrypoint.iter().chain(img.cmd.iter()).cloned().collect(),
    }
}

/// GPUs on the server a replica runs on (other servers report theirs in their health).
fn gpu_access(app: &App, node: &NodeHandle) -> (Option<Vec<DeviceRequest>>, Option<Vec<bollard::models::DeviceMapping>>) {
    let (nvidia, dri) = if node.local {
        (app.db.state::<String>("gpu:nvidia").as_deref() == Some("1"), std::path::Path::new("/dev/dri").exists())
    } else {
        let info: serde_json::Value =
            app.db.row("SELECT info FROM nodes WHERE id = ?1", [&node.id], |r| r.get::<_, String>(0)).ok().flatten().and_then(|i| serde_json::from_str(&i).ok()).unwrap_or_default();
        (info["nvidia"].as_bool().unwrap_or(false), info["dri"].as_bool().unwrap_or(false))
    };
    if nvidia {
        let req = DeviceRequest { driver: Some("nvidia".into()), count: Some(-1), capabilities: Some(vec![vec!["gpu".into()]]), ..Default::default() };
        (Some(vec![req]), None)
    } else if dri {
        let dev = bollard::models::DeviceMapping {
            path_on_host: Some("/dev/dri".into()),
            path_in_container: Some("/dev/dri".into()),
            cgroup_permissions: Some("rwm".into()),
        };
        (None, Some(vec![dev]))
    } else {
        (None, None)
    }
}

fn mount(typ: MountTypeEnum, source: &str, target: &str, read_only: bool) -> DockerMount {
    DockerMount { typ: Some(typ), source: Some(source.into()), target: Some(target.into()), read_only: Some(read_only), ..Default::default() }
}

fn bind(source: &str, target: &str, read_only: bool) -> DockerMount {
    mount(MountTypeEnum::BIND, source, target, read_only)
}

/// `script` run by `sh` through Tugport's init, so with the service's environment.
fn init_sh(script: &str) -> Vec<String> {
    [secrets::INIT_PATH, "exec", "--", "sh", "-c", script].map(String::from).into()
}

/// None for an empty list or map, which Docker reads as "leave the default".
fn non_empty<C: Default + PartialEq>(c: C) -> Option<C> {
    (c != C::default()).then_some(c)
}

/// Names other services of the project reach a service's replicas by.
fn aliases(s: &Service) -> Vec<String> {
    std::iter::once(&s.name).chain(&s.spec.aliases).cloned().collect()
}

/// MiB in bytes.
fn mib(m: u64) -> i64 {
    i64::try_from(m.saturating_mul(1024 * 1024)).unwrap_or(i64::MAX)
}

/// Seconds in Docker's nanoseconds.
fn nanos(secs: u32) -> i64 {
    i64::from(secs) * 1_000_000_000
}

pub fn container_name(s: &Service, deploy_id: &str, replica: u32) -> String {
    format!("{}-{}-{replica}", s.slug(), deploy_id.get(..6).unwrap_or(deploy_id))
}

/// Like other platforms, tell the app which port the proxy expects it on.
pub fn add_port(app: &App, s: &Service, env: &mut std::collections::BTreeMap<String, zeroize::Zeroizing<String>>) -> Result<()> {
    if !env.contains_key("PORT")
        && let Some(d) = store::domains(app, Some(&s.id))?.first()
    {
        env.insert("PORT".into(), zeroize::Zeroizing::new(d.port.to_string()));
    }
    Ok(())
}

/// The Docker health check of a service: its command, explicitly none, or the image's own.
fn healthcheck(h: &Health) -> Option<HealthConfig> {
    match h.kind {
        HealthKind::Command => h.command.as_ref().map(|c| HealthConfig {
            test: Some(std::iter::once("CMD".to_string()).chain(init_sh(c)).collect()),
            interval: Some(nanos(h.interval_s)),
            timeout: Some(nanos(h.timeout_s)),
            retries: Some(h.retries.into()),
            start_period: Some(nanos(h.start_period_s)),
            start_interval: Some(nanos(1)),
        }),
        HealthKind::None => Some(HealthConfig { test: Some(vec!["NONE".into()]), ..Default::default() }),
        _ => None,
    }
}

/// Create (not start) one replica of a service running `image` on `node`.
#[allow(clippy::too_many_arguments)]
pub async fn create_replica(app: &App, node: &NodeHandle, s: &Service, deploy_id: &str, replica: u32, image: &str, img: &ImageInfo, network: &str, publish_ports: bool) -> Result<String> {
    let spec = &s.spec;
    let name = container_name(s, deploy_id, replica);
    let multi = crate::nodes::mesh_enabled(app);
    if multi && !spec.host_network {
        ensure_ingress(&node.docker).await?;
    }
    let (env_file, files) = crate::nodes::put_secrets(app, node, &name, s).await?;
    let mut mounts = vec![bind(&app.paths.init_binary().to_string_lossy(), secrets::INIT_PATH, true), bind(&env_file, secrets::ENV_PATH, true)];
    mounts.extend(files.iter().map(|(host, target)| bind(host, target, true)));
    mounts.extend(spec.mounts.iter().filter_map(|m| match m {
        Mount::Volume { name, target, readonly } => Some(mount(MountTypeEnum::VOLUME, name, target, *readonly)),
        Mount::Bind { source, target, readonly } => Some(bind(source, target, *readonly)),
        Mount::File { .. } => None,
    }));
    if spec.docker_socket {
        mounts.push(bind("/var/run/docker.sock", "/var/run/docker.sock", false));
    }

    let run = final_command(spec, img);
    if run.is_empty() {
        bail!("the image has no command to run; set one on the service");
    }
    let (exposed, port_bindings): (HashMap<_, HashMap<(), ()>>, HashMap<_, _>) = spec
        .ports
        .iter()
        .filter(|_| publish_ports)
        .map(|p| {
            let key = format!("{}/{}", p.container, if p.protocol == Protocol::Udp { "udp" } else { "tcp" });
            ((key.clone(), HashMap::new()), (key, Some(vec![PortBinding { host_ip: None, host_port: Some(p.host.to_string()) }])))
        })
        .unzip();

    let (device_requests, devices) = if spec.gpu { gpu_access(app, node) } else { (None, None) };
    let labels = labels(s, deploy_id, &node.id).into_iter().chain([(L_REPLICA, replica.to_string())]).map(|(k, v)| (k.to_string(), v)).collect();
    let host_config = HostConfig {
        mounts: Some(mounts),
        restart_policy: Some(RestartPolicy { name: Some(RestartPolicyNameEnum::UNLESS_STOPPED), maximum_retry_count: None }),
        log_config: Some(HostConfigLogConfig {
            typ: Some("json-file".into()),
            config: Some(LOG_LIMITS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()),
        }),
        nano_cpus: spec.resources.cpus.map(|c| (c * 1e9) as i64),
        memory: spec.resources.memory_mb.map(mib),
        memory_reservation: spec.resources.memory_reservation_mb.map(mib),
        cap_add: non_empty(spec.cap_add.clone()),
        cap_drop: non_empty(spec.cap_drop.clone()),
        group_add: non_empty(spec.groups.clone()),
        sysctls: non_empty(spec.sysctls.clone().into_iter().collect()),
        privileged: Some(spec.privileged),
        // With several networks Docker cannot attach an overlay that has no containers on
        // this server yet as the first one; the local ingress bridge always can be.
        network_mode: Some(if spec.host_network {
            "host".into()
        } else if multi {
            INGRESS.into()
        } else {
            network.to_string()
        }),
        security_opt: spec.no_new_privileges.then(|| vec!["no-new-privileges:true".into()]),
        port_bindings: non_empty(port_bindings),
        device_requests,
        devices,
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".into()]),
        ..Default::default()
    };
    let networking = (!spec.host_network).then(|| {
        let aliases = aliases(s);
        let mut endpoints: HashMap<String, EndpointSettings> = std::iter::once(network)
            .chain(spec.networks.iter().map(String::as_str))
            .map(|n| (n.to_string(), EndpointSettings { aliases: Some(aliases.clone()), ..Default::default() }))
            .collect();
        if multi {
            endpoints.insert(INGRESS.to_string(), EndpointSettings::default());
        }
        NetworkingConfig { endpoints_config: Some(endpoints) }
    });
    let config = ContainerCreateBody {
        image: Some(image.to_string()),
        entrypoint: Some(vec![secrets::INIT_PATH.to_string()]),
        cmd: Some(std::iter::once("--".to_string()).chain(run).collect()),
        user: spec.user.clone(),
        working_dir: spec.working_dir.clone(),
        labels: Some(labels),
        healthcheck: healthcheck(&spec.health),
        exposed_ports: non_empty(exposed),
        env: Some(vec![format!("TUGPORT_SERVICE={}", s.name), format!("TUGPORT_PROJECT={}", s.project)]),
        host_config: Some(host_config),
        networking_config: networking,
        ..Default::default()
    };
    // A leftover container with the same name (from a crashed deploy) would block creation.
    let _ = force_remove(&node.docker, &name).await;
    let created = node
        .docker
        .create_container(Some(CreateContainerOptions { name: Some(name.clone()), ..Default::default() }), config)
        .await
        .with_context(|| format!("creating container {name}"))?;
    Ok(created.id)
}

/// Create and start one replica (ports unpublished) and wait until it is healthy; it is
/// removed again if it never is.
#[allow(clippy::too_many_arguments)]
pub async fn start_healthy(app: &App, node: &NodeHandle, s: &Service, deploy_id: &str, replica: u32, image: &str, img: &ImageInfo, network: &str, timeout: Duration) -> Result<String> {
    let id = create_replica(app, node, s, deploy_id, replica, image, img, network, false).await?;
    let started = async {
        start(app, &node.id, &id).await?;
        wait_healthy(app, s, &node.id, &id, network, timeout, img.has_healthcheck, &|_| {}).await
    };
    if let Err(e) = started.await {
        let _ = remove(app, &node.id, &id, &container_name(s, deploy_id, replica)).await;
        return Err(e);
    }
    Ok(id)
}

/// Calls that wait on the daemon's own work (starting, stopping, removing, loading images) can
/// take minutes on a host with saturated disks; bollard's 2-minute default would report a failure
/// while Docker is still doing the work.
pub const SLOW: Duration = Duration::from_secs(600);

pub fn slow(docker: bollard::Docker) -> bollard::Docker {
    docker.with_timeout(SLOW)
}

fn gone(e: &bollard::errors::Error) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code: 404, .. })
}

pub async fn start(app: &App, node: &str, id: &str) -> Result<()> {
    slow(app.docker_on(node)?).start_container(id, None::<bollard::query_parameters::StartContainerOptions>).await.with_context(|| format!("starting {id}"))?;
    Ok(())
}

pub async fn stop(app: &App, node: &str, id: &str, timeout_s: i32) -> Result<()> {
    stop_on(&app.docker_on(node)?, id, timeout_s).await
}

/// Stop a container. When the stop call fails (a client timeout while the daemon is busy, say),
/// kill it and succeed if it is down after all: callers must not act on a container that may still run.
async fn stop_on(docker: &bollard::Docker, id: &str, timeout_s: i32) -> Result<()> {
    let grace = Duration::from_secs(timeout_s.max(0) as u64);
    let docker = docker.clone().with_timeout(SLOW + grace);
    let err = match docker.stop_container(id, Some(StopContainerOptions { t: Some(timeout_s), ..Default::default() })).await {
        Ok(()) | Err(bollard::errors::Error::DockerResponseServerError { status_code: 304 | 404, .. }) => return Ok(()),
        Err(e) => e,
    };
    tracing::warn!("stopping {id}: {err}; killing it");
    let _ = docker.kill_container(id, None::<bollard::query_parameters::KillContainerOptions>).await;
    match docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await {
        Ok(i) if i.state.as_ref().and_then(|s| s.running) != Some(true) => Ok(()),
        Err(e) if gone(&e) => Ok(()),
        _ => Err(anyhow::Error::from(err).context(format!("{id} is still running"))),
    }
}

pub async fn restart(app: &App, node: &str, id: &str) -> Result<()> {
    slow(app.docker_on(node)?).restart_container(id, None::<bollard::query_parameters::RestartContainerOptions>).await?;
    Ok(())
}

async fn force_remove(docker: &bollard::Docker, id: &str) -> Result<()> {
    match slow(docker.clone()).remove_container(id, Some(RemoveContainerOptions { force: true, ..Default::default() })).await {
        // 409: Docker is already removing it.
        Ok(()) | Err(bollard::errors::Error::DockerResponseServerError { status_code: 404 | 409, .. }) => Ok(()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("removing {id}"))),
    }
}

pub async fn remove(app: &App, node: &str, id: &str, name: &str) -> Result<()> {
    let removed = force_remove(&app.docker_on(node)?, id).await;
    crate::nodes::forget_secrets(app, node, name).await;
    removed
}

pub async fn stop_and_remove(app: &App, r: &Replica, timeout_s: i32) -> Result<()> {
    stop(app, &r.node, &r.id, timeout_s).await?;
    remove(app, &r.node, &r.id, &r.name).await
}

pub async fn remove_service(app: &App, s: &Service, volumes: bool) -> Result<()> {
    if s.kind == "compose" {
        crate::compose::down(s, volumes).await?;
    }
    for r in replicas(app, &s.id).await? {
        stop_and_remove(app, &r, 20).await?;
    }
    if volumes {
        let node = app.db.get_state(&format!("pin:{}", s.id))?.unwrap_or_else(|| "local".into());
        let docker = app.docker_on(&node)?;
        for m in &s.spec.mounts {
            if let Mount::Volume { name, .. } = m {
                let _ = docker.remove_volume(name, Some(RemoveVolumeOptions { force: true })).await;
            }
        }
    }
    for key in ["pin", "nodes", "stopped", "hook", "live", "asleep", "exposed", "active", "scale", "replica"] {
        let _ = app.db.del_state(&format!("{key}:{}", s.id));
    }
    let _ = std::fs::remove_dir_all(app.paths.sources().join(&s.id));
    Ok(())
}

/// Where the replica's own server can reach it: the ingress bridge, else `network`.
async fn container_ip(docker: &bollard::Docker, id: &str, network: &str) -> Option<String> {
    let i = docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await.ok()?;
    let nets = i.network_settings?.networks?;
    [INGRESS, network].iter().find_map(|n| nets.get(*n)?.ip_address.clone().filter(|ip| !ip.is_empty()))
}

/// HTTP (with `path`) or TCP check from the replica's own server.
pub async fn probe(app: &App, node: &str, ip: &str, port: u16, path: Option<&str>, strict: bool) -> bool {
    match app.node(node).filter(|n| !n.local) {
        Some(n) => {
            let body = serde_json::json!({ "ip": ip, "port": port, "path": path, "strict": strict });
            crate::nodes::agent(&n, reqwest::Method::POST, "/agent/check", Some(body)).await.ok().and_then(|v| v["ok"].as_bool()).unwrap_or(false)
        }
        None => match path {
            Some(p) => http_ok(ip, port, p, strict).await,
            None => tokio::net::TcpStream::connect((ip, port)).await.is_ok(),
        },
    }
}

/// Wait until a replica is healthy. Errors carry the reason (exit code, failing check).
#[allow(clippy::too_many_arguments)]
pub async fn wait_healthy(app: &App, s: &Service, node: &str, id: &str, network: &str, timeout: Duration, has_image_check: bool, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let spec = &s.spec;
    let docker = app.docker_on(node)?;
    let started = std::time::Instant::now();
    let domain_port = store::domains(app, Some(&s.id)).ok().and_then(|d| d.first().map(|d| d.port));
    let settle = Duration::from_secs(spec.health.start_period_s.max(3).into());
    let uses_docker_check = spec.health.kind == HealthKind::Command || (spec.health.kind == HealthKind::Auto && has_image_check);
    // (port, HTTP path, whether only 2xx/3xx count) of Tugport's own check.
    let (port, path, strict) = match spec.health.kind {
        HealthKind::Tcp => (spec.health.port.or(domain_port), None, false),
        HealthKind::Http => (spec.health.port.or(domain_port), Some(spec.health.path.as_str()), true),
        // A web service counts as up once it answers at all.
        _ => (domain_port, Some(spec.health.path.as_str()), false),
    };
    let mut announced = false;
    loop {
        let i = docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await?;
        let state = i.state.unwrap_or_default();
        let running = state.running == Some(true) && state.restarting != Some(true);
        if !running && started.elapsed() > Duration::from_secs(1) {
            let oom = if state.oom_killed == Some(true) { " (out of memory)" } else { "" };
            bail!("the container exited with code {}{oom}", state.exit_code.unwrap_or(0));
        }
        let healthy = running
            && if uses_docker_check {
                match state.health.as_ref().and_then(|h| h.status) {
                    Some(HealthStatusEnum::HEALTHY) => true,
                    Some(HealthStatusEnum::UNHEALTHY) => bail!("the health check reports unhealthy"),
                    _ => false,
                }
            } else if spec.health.kind == HealthKind::None {
                true
            } else {
                match (container_ip(&docker, id, network).await, port) {
                    (Some(ip), Some(port)) => probe(app, node, &ip, port, path, strict).await,
                    _ if spec.health.kind == HealthKind::Http => bail!("HTTP health checks need a port"),
                    _ => started.elapsed() >= settle,
                }
            };
        if healthy {
            return Ok(());
        }
        if started.elapsed() > timeout {
            return Err(anyhow!("not healthy after {}s", timeout.as_secs()));
        }
        if !announced && started.elapsed() > Duration::from_secs(10) {
            log("waiting for the new version to become healthy...");
            announced = true;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
}

pub async fn http_ok(ip: &str, port: u16, path: &str, strict: bool) -> bool {
    // One client for every probe: building one loads the TLS roots.
    static CLIENT: std::sync::LazyLock<Option<reqwest::Client>> = std::sync::LazyLock::new(|| crate::util::http(5).redirect(reqwest::redirect::Policy::none()).build().ok());
    let Some(client) = CLIENT.as_ref() else { return false };
    match client.get(format!("http://{ip}:{port}{path}")).send().await {
        Ok(r) if strict => r.status().is_success() || r.status().is_redirection(),
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

/// Run a shell command in a replica (through Tugport's init, so with its environment) and
/// return its exit code and output.
pub async fn exec(app: &App, r: &Replica, script: &str) -> Result<(i64, String)> {
    let docker = app.docker_on(&r.node)?;
    let config = bollard::models::ExecConfig { attach_stdout: Some(true), attach_stderr: Some(true), cmd: Some(init_sh(script)), ..Default::default() };
    let created = docker.create_exec(&r.id, config).await.with_context(|| format!("running a command in {}", r.name))?;
    let text = match docker.start_exec(&created.id, None).await? {
        bollard::exec::StartExecResults::Attached { output, .. } => text(output).await,
        bollard::exec::StartExecResults::Detached => String::new(),
    };
    let code = docker.inspect_exec(&created.id).await?.exit_code.unwrap_or(-1);
    Ok((code, text))
}

/// Tail of a container's output, for explaining failed deploys.
pub async fn tail_logs(app: &App, node: &str, id: &str, lines: usize) -> Vec<String> {
    let Ok(docker) = app.docker_on(node) else { return vec![] };
    let opts = bollard::query_parameters::LogsOptions { stdout: true, stderr: true, tail: lines.to_string(), ..Default::default() };
    text(docker.logs(id, Some(opts))).await.lines().map(String::from).collect()
}

/// A container's output stream as text, up to its end or first error.
async fn text(mut output: impl futures_util::Stream<Item = Result<bollard::container::LogOutput, bollard::errors::Error>> + Unpin) -> String {
    let mut text = String::new();
    while let Some(Ok(chunk)) = output.next().await {
        text.push_str(&String::from_utf8_lossy(&chunk.into_bytes()));
    }
    text
}

/// Detect GPUs once at startup so containers can be given access.
pub async fn detect_gpu(app: &AppState) {
    if let Ok(info) = app.docker.info().await {
        let nvidia = info.runtimes.as_ref().is_some_and(|r| r.contains_key("nvidia"));
        let _ = app.db.set_state("gpu:nvidia", if nvidia { "1" } else { "0" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Docker API that fails every stop with `stop_status`; a kill stops the container if `killable`.
    async fn fake_docker(stop_status: u16, killable: bool) -> (bollard::Docker, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::http::{Method, StatusCode, Uri};
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(vec![]));
        let running = Arc::new(Mutex::new(true));
        let (seen, up) = (calls.clone(), running.clone());
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri| {
            let (seen, up) = (seen.clone(), up.clone());
            async move {
                let path = uri.path().to_string();
                seen.lock().unwrap().push(format!("{method} {}", path.rsplit('/').next().unwrap_or("")));
                if path.ends_with("/stop") {
                    (StatusCode::from_u16(stop_status).unwrap(), r#"{"message":"Timeout error"}"#.to_string())
                } else if path.ends_with("/kill") {
                    *up.lock().unwrap() = !killable;
                    (StatusCode::NO_CONTENT, String::new())
                } else {
                    (StatusCode::OK, format!(r#"{{"Id":"c1","State":{{"Running":{}}}}}"#, up.lock().unwrap()))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let client = Arc::new(hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http());
        let docker = bollard::Docker::connect_with_custom_transport(
            move |req: bollard::BollardRequest| {
                let client = client.clone();
                async move { client.request(req).await.map_err(bollard::errors::Error::from) }
            },
            Some(format!("http://{addr}")),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap();
        (docker, calls)
    }

    #[tokio::test]
    async fn a_failed_stop_kills_and_succeeds_once_the_container_is_down() {
        let (docker, calls) = fake_docker(500, true).await;
        stop_on(&docker, "c1", 30).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), ["POST stop", "POST kill", "GET json"]);
    }

    #[tokio::test]
    async fn a_container_that_keeps_running_is_an_error() {
        let (docker, _) = fake_docker(500, false).await;
        let e = stop_on(&docker, "c1", 30).await.unwrap_err();
        assert!(format!("{e:#}").contains("still running"), "{e:#}");
    }

    #[tokio::test]
    async fn stopped_or_missing_containers_need_no_kill() {
        for status in [304, 404] {
            let (docker, calls) = fake_docker(status, false).await;
            stop_on(&docker, "c1", 30).await.unwrap();
            assert_eq!(*calls.lock().unwrap(), ["POST stop"]);
        }
    }

    #[test]
    fn health_commands_run_through_init() {
        let h = Health { kind: HealthKind::Command, command: Some("curl -f localhost".into()), ..Default::default() };
        let test = healthcheck(&h).and_then(|c| c.test).unwrap_or_default();
        assert_eq!(test, ["CMD", secrets::INIT_PATH, "exec", "--", "sh", "-c", "curl -f localhost"]);
        assert!(healthcheck(&Health::default()).is_none());
        assert_eq!(non_empty(Vec::<String>::new()), None);
        assert_eq!(listed_name(Some(vec!["/web-1".into()])), "web-1");
    }
}
