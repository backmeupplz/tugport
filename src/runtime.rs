//! Turning services into running containers: networks, container configs, health checks.

use crate::app::{App, AppState};
use crate::model::{HealthKind, Mount, Protocol, ServiceSpec};
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
pub const L_REPLICA: &str = "tugport.replica";
pub const L_NODE: &str = "tugport.node";

pub fn network_name(project: &str) -> String {
    format!("tp-{project}")
}

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
    /// Compose service name for containers of Compose stacks.
    pub compose_service: Option<String>,
}

fn label_filter(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
    HashMap::from([("label".into(), pairs.iter().map(|(k, v)| format!("{k}={v}")).collect())])
}

/// Containers that belong to a service, newest first.
pub async fn replicas(app: &App, service_id: &str) -> Result<Vec<Replica>> {
    let list = app
        .docker
        .list_containers(Some(ListContainersOptions { all: true, filters: Some(label_filter(&[(L_SERVICE, service_id)])), ..Default::default() }))
        .await?;
    let mut out = vec![];
    for c in list {
        let labels = c.labels.clone().unwrap_or_default();
        let ip = c
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.as_ref())
            .and_then(|n| n.values().find_map(|e| e.ip_address.clone().filter(|ip| !ip.is_empty())));
        let health = c.status.as_deref().and_then(|s| {
            ["(healthy)", "(unhealthy)", "(health: starting)"].iter().find(|h| s.contains(*h)).map(|h| h.trim_matches(['(', ')']).replace("health: ", ""))
        });
        out.push(Replica {
            id: c.id.unwrap_or_default(),
            name: c.names.and_then(|n| n.first().cloned()).unwrap_or_default().trim_start_matches('/').to_string(),
            deploy: labels.get(L_DEPLOY).cloned().unwrap_or_default(),
            replica: labels.get(L_REPLICA).and_then(|r| r.parse().ok()).unwrap_or(0),
            state: c.state.map(|s| s.to_string()).unwrap_or_default(),
            health,
            ip,
            image: c.image.unwrap_or_default(),
            created: c.created.unwrap_or_default(),
            node: labels.get(L_NODE).cloned().unwrap_or_else(|| "local".into()),
            compose_service: labels.get("com.docker.compose.service").cloned(),
        });
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then(a.replica.cmp(&b.replica)));
    Ok(out)
}

/// The deploy currently receiving traffic.
pub fn live_deploy(app: &App, service_id: &str) -> Option<String> {
    app.db.get_state(&format!("live:{service_id}")).ok().flatten()
}

pub fn set_live_deploy(app: &App, service_id: &str, deploy_id: &str) -> Result<()> {
    app.db.set_state(&format!("live:{service_id}"), deploy_id)
}

#[derive(Serialize)]
pub struct Status {
    pub state: &'static str,
    pub running: usize,
    pub desired: u32,
    pub live_deploy: Option<String>,
    pub replicas: Vec<Replica>,
}

pub async fn service_status(app: &App, s: &Service) -> serde_json::Value {
    let reps = replicas(app, &s.id).await.unwrap_or_default();
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
    let name = network_name(project);
    if app.docker.inspect_network(&name, None::<bollard::query_parameters::InspectNetworkOptions>).await.is_ok() {
        return Ok(name);
    }
    let labels = HashMap::from([(L_MANAGED.to_string(), "true".to_string())]);
    app.docker
        .create_network(NetworkCreateRequest { name: name.clone(), driver: Some("bridge".into()), labels: Some(labels), ..Default::default() })
        .await
        .with_context(|| format!("creating network {name}"))?;
    Ok(name)
}

pub async fn remove_project_network(app: &App, p: &Project) {
    let _ = app.docker.remove_network(&network_name(&p.name)).await;
}

pub async fn pull(app: &App, image: &str, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let (repo, tag) = split_image(image);
    let creds = registry_credentials(app, image)?;
    let mut stream = app.docker.create_image(
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

fn registry_credentials(app: &App, image: &str) -> Result<Option<bollard::auth::DockerCredentials>> {
    let host = registry_host(image);
    let row: Option<(String, Vec<u8>)> = {
        use rusqlite::OptionalExtension;
        app.db
            .conn()
            .query_row("SELECT username, password FROM registries WHERE host = ?1", [host], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?
    };
    Ok(match row {
        Some((user, pw)) => Some(bollard::auth::DockerCredentials {
            username: Some(user),
            password: Some(app.keys.open_str(&format!("registry:{host}"), &pw)?.to_string()),
            serveraddress: Some(host.to_string()),
            ..Default::default()
        }),
        None => None,
    })
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
pub fn final_command(spec: &ServiceSpec, img: &ImageInfo) -> Vec<String> {
    match (&spec.entrypoint, &spec.command) {
        (Some(ep), cmd) => ep.iter().chain(cmd.iter().flatten()).cloned().collect(),
        (None, Some(cmd)) => img.entrypoint.iter().chain(cmd.iter()).cloned().collect(),
        (None, None) => img.entrypoint.iter().chain(img.cmd.iter()).cloned().collect(),
    }
}

fn gpu_access(app: &App) -> (Option<Vec<DeviceRequest>>, Option<Vec<bollard::models::DeviceMapping>>) {
    if app.db.get_state("gpu:nvidia").ok().flatten().as_deref() == Some("1") {
        let req = DeviceRequest { driver: Some("nvidia".into()), count: Some(-1), capabilities: Some(vec![vec!["gpu".into()]]), ..Default::default() };
        (Some(vec![req]), None)
    } else if std::path::Path::new("/dev/dri").exists() {
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

fn bind(source: &str, target: &str, read_only: bool) -> DockerMount {
    DockerMount { typ: Some(MountTypeEnum::BIND), source: Some(source.into()), target: Some(target.into()), read_only: Some(read_only), ..Default::default() }
}

pub fn container_name(s: &Service, deploy_id: &str, replica: u32) -> String {
    format!("{}-{}-{replica}", s.slug(), &deploy_id[..deploy_id.len().min(6)])
}

/// Create (not start) one replica of a service running `image`.
#[allow(clippy::too_many_arguments)]
pub async fn create_replica(app: &App, s: &Service, deploy_id: &str, replica: u32, image: &str, img: &ImageInfo, network: &str, publish_ports: bool) -> Result<String> {
    let spec = &s.spec;
    let name = container_name(s, deploy_id, replica);
    let mut env = store::resolved_env(app, s)?;
    // Like other platforms, tell the app which port the proxy expects it on.
    if !env.contains_key("PORT") {
        if let Some(d) = store::domains(app, Some(&s.id))?.first() {
            env.insert("PORT".into(), zeroize::Zeroizing::new(d.port.to_string()));
        }
    }
    let env_file = secrets::materialize(app, &name, &env)?;
    let mut mounts = vec![
        bind(&app.paths.init_binary().to_string_lossy(), secrets::INIT_PATH, true),
        bind(&env_file.to_string_lossy(), secrets::ENV_PATH, true),
    ];
    let files = store::files(app, &s.id)?;
    for (host, target) in secrets::materialize_files(app, &name, &files)? {
        mounts.push(bind(&host.to_string_lossy(), &target, true));
    }
    for m in &spec.mounts {
        match m {
            Mount::Volume { name, target, readonly } => mounts.push(DockerMount {
                typ: Some(MountTypeEnum::VOLUME),
                source: Some(name.clone()),
                target: Some(target.clone()),
                read_only: Some(*readonly),
                ..Default::default()
            }),
            Mount::Bind { source, target, readonly } => mounts.push(bind(source, target, *readonly)),
            Mount::File { .. } => {}
        }
    }
    if spec.docker_socket {
        mounts.push(bind("/var/run/docker.sock", "/var/run/docker.sock", false));
    }

    let mut cmd = vec!["--".to_string()];
    cmd.extend(final_command(spec, img));
    if cmd.len() == 1 {
        bail!("the image has no command to run; set one on the service");
    }

    let healthcheck = match spec.health.kind {
        HealthKind::Command => spec.health.command.as_ref().map(|c| HealthConfig {
            test: Some(vec!["CMD".into(), secrets::INIT_PATH.into(), "exec".into(), "--".into(), "sh".into(), "-c".into(), c.clone()]),
            interval: Some(spec.health.interval_s as i64 * 1_000_000_000),
            timeout: Some(spec.health.timeout_s as i64 * 1_000_000_000),
            retries: Some(spec.health.retries as i64),
            start_period: Some(spec.health.start_period_s as i64 * 1_000_000_000),
            start_interval: Some(1_000_000_000),
        }),
        HealthKind::None => Some(HealthConfig { test: Some(vec!["NONE".into()]), ..Default::default() }),
        _ => None,
    };

    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    let mut exposed: HashMap<String, HashMap<(), ()>> = HashMap::new();
    if publish_ports {
        for p in &spec.ports {
            let key = format!("{}/{}", p.container, if p.protocol == Protocol::Udp { "udp" } else { "tcp" });
            exposed.insert(key.clone(), HashMap::new());
            port_bindings.insert(key, Some(vec![PortBinding { host_ip: None, host_port: Some(p.host.to_string()) }]));
        }
    }

    let (device_requests, devices) = if spec.gpu { gpu_access(app) } else { (None, None) };
    let mut security_opt = vec![];
    if spec.no_new_privileges {
        security_opt.push("no-new-privileges:true".to_string());
    }
    let labels = HashMap::from([
        (L_MANAGED.to_string(), "true".to_string()),
        (L_PROJECT.to_string(), s.project_id.clone()),
        (L_SERVICE.to_string(), s.id.clone()),
        (L_DEPLOY.to_string(), deploy_id.to_string()),
        (L_REPLICA.to_string(), replica.to_string()),
        (L_NODE.to_string(), "local".to_string()),
    ]);
    let host_config = HostConfig {
        mounts: Some(mounts),
        restart_policy: Some(RestartPolicy { name: Some(RestartPolicyNameEnum::UNLESS_STOPPED), maximum_retry_count: None }),
        log_config: Some(HostConfigLogConfig {
            typ: Some("json-file".into()),
            config: Some(HashMap::from([("max-size".into(), "10m".into()), ("max-file".into(), "3".into())])),
        }),
        nano_cpus: spec.resources.cpus.map(|c| (c * 1e9) as i64),
        memory: spec.resources.memory_mb.map(|m| (m * 1024 * 1024) as i64),
        memory_reservation: spec.resources.memory_reservation_mb.map(|m| (m * 1024 * 1024) as i64),
        cap_add: (!spec.cap_add.is_empty()).then(|| spec.cap_add.clone()),
        cap_drop: (!spec.cap_drop.is_empty()).then(|| spec.cap_drop.clone()),
        group_add: (!spec.groups.is_empty()).then(|| spec.groups.clone()),
        sysctls: (!spec.sysctls.is_empty()).then(|| spec.sysctls.clone().into_iter().collect()),
        privileged: Some(spec.privileged),
        network_mode: Some(if spec.host_network { "host".into() } else { network.to_string() }),
        security_opt: (!security_opt.is_empty()).then_some(security_opt),
        port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
        device_requests,
        devices,
        extra_hosts: Some(vec!["host.docker.internal:host-gateway".into()]),
        ..Default::default()
    };
    let networking = (!spec.host_network).then(|| {
        let mut aliases = vec![s.name.clone()];
        aliases.extend(spec.aliases.iter().cloned());
        let mut endpoints = HashMap::from([(network.to_string(), EndpointSettings { aliases: Some(aliases.clone()), ..Default::default() })]);
        for extra in &spec.networks {
            endpoints.insert(extra.clone(), EndpointSettings { aliases: Some(aliases.clone()), ..Default::default() });
        }
        NetworkingConfig { endpoints_config: Some(endpoints) }
    });
    let config = ContainerCreateBody {
        image: Some(image.to_string()),
        entrypoint: Some(vec![secrets::INIT_PATH.to_string()]),
        cmd: Some(cmd),
        user: spec.user.clone(),
        working_dir: spec.working_dir.clone(),
        labels: Some(labels),
        healthcheck,
        exposed_ports: (!exposed.is_empty()).then_some(exposed),
        env: Some(vec![format!("TUGPORT_SERVICE={}", s.name), format!("TUGPORT_PROJECT={}", s.project)]),
        host_config: Some(host_config),
        networking_config: networking,
        ..Default::default()
    };
    // A leftover container with the same name (from a crashed deploy) would block creation.
    let _ = app.docker.remove_container(&name, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
    let created = app
        .docker
        .create_container(Some(CreateContainerOptions { name: Some(name.clone()), ..Default::default() }), config)
        .await
        .with_context(|| format!("creating container {name}"))?;
    Ok(created.id)
}

pub async fn start(app: &App, id: &str) -> Result<()> {
    app.docker.start_container(id, None::<bollard::query_parameters::StartContainerOptions>).await.with_context(|| format!("starting {id}"))?;
    Ok(())
}

pub async fn stop(app: &App, id: &str, timeout_s: i32) -> Result<()> {
    match app.docker.stop_container(id, Some(StopContainerOptions { t: Some(timeout_s), ..Default::default() })).await {
        Ok(()) => Ok(()),
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 304 | 404, .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub async fn remove(app: &App, id: &str, name: &str) -> Result<()> {
    let _ = app.docker.remove_container(id, Some(RemoveContainerOptions { force: true, ..Default::default() })).await;
    secrets::forget(app, name);
    Ok(())
}

pub async fn stop_and_remove(app: &App, r: &Replica, timeout_s: i32) -> Result<()> {
    stop(app, &r.id, timeout_s).await?;
    remove(app, &r.id, &r.name).await
}

pub async fn remove_service(app: &App, s: &Service, volumes: bool) -> Result<()> {
    if s.kind == "compose" {
        crate::compose::down(app, s, volumes).await?;
    }
    for r in replicas(app, &s.id).await? {
        stop_and_remove(app, &r, 20).await?;
    }
    if volumes {
        for m in &s.spec.mounts {
            if let Mount::Volume { name, .. } = m {
                let _ = app.docker.remove_volume(name, Some(RemoveVolumeOptions { force: true })).await;
            }
        }
    }
    let _ = std::fs::remove_dir_all(app.paths.sources().join(&s.id));
    Ok(())
}

pub async fn container_ip(app: &App, id: &str, network: &str) -> Option<String> {
    let i = app.docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await.ok()?;
    i.network_settings?.networks?.get(network)?.ip_address.clone().filter(|ip| !ip.is_empty())
}

/// Wait until a replica is healthy. Errors carry the reason (exit code, failing check).
pub async fn wait_healthy(app: &App, s: &Service, id: &str, network: &str, timeout: Duration, has_image_check: bool, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    let spec = &s.spec;
    let started = std::time::Instant::now();
    let domain_port = store::domains(app, Some(&s.id)).ok().and_then(|d| d.first().map(|d| d.port));
    let settle = Duration::from_secs(spec.health.start_period_s.max(3) as u64);
    let mut announced = false;
    loop {
        let i = app.docker.inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>).await?;
        let state = i.state.unwrap_or_default();
        if state.running != Some(true) || state.restarting == Some(true) {
            let code = state.exit_code.unwrap_or(0);
            let oom = if state.oom_killed == Some(true) { " (out of memory)" } else { "" };
            if started.elapsed() > Duration::from_secs(1) {
                bail!("the container exited with code {code}{oom}");
            }
        } else {
            let docker_health = state.health.as_ref().and_then(|h| h.status);
            let uses_docker_check = matches!(spec.health.kind, HealthKind::Command) || (spec.health.kind == HealthKind::Auto && has_image_check);
            let healthy = if uses_docker_check {
                match docker_health {
                    Some(HealthStatusEnum::HEALTHY) => true,
                    Some(HealthStatusEnum::UNHEALTHY) => bail!("the health check reports unhealthy"),
                    _ => false,
                }
            } else {
                match spec.health.kind {
                    HealthKind::None => true,
                    HealthKind::Tcp => match (container_ip(app, id, network).await, spec.health.port.or(domain_port)) {
                        (Some(ip), Some(port)) => tokio::net::TcpStream::connect((ip.as_str(), port)).await.is_ok(),
                        _ => started.elapsed() >= settle,
                    },
                    HealthKind::Http => match (container_ip(app, id, network).await, spec.health.port.or(domain_port)) {
                        (Some(ip), Some(port)) => http_ok(&ip, port, &spec.health.path, true).await,
                        _ => bail!("HTTP health checks need a port"),
                    },
                    _ => match (container_ip(app, id, network).await, domain_port) {
                        // A web service counts as up once it answers at all.
                        (Some(ip), Some(port)) => http_ok(&ip, port, &spec.health.path, false).await,
                        _ => started.elapsed() >= settle,
                    },
                }
            };
            if healthy {
                return Ok(());
            }
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

async fn http_ok(ip: &str, port: u16, path: &str, strict: bool) -> bool {
    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build() else {
        return false;
    };
    match client.get(format!("http://{ip}:{port}{path}")).send().await {
        Ok(r) if strict => r.status().is_success() || r.status().is_redirection(),
        Ok(r) => r.status().as_u16() < 500,
        Err(_) => false,
    }
}

/// Tail of a container's output, for explaining failed deploys.
pub async fn tail_logs(app: &App, id: &str, lines: usize) -> Vec<String> {
    let mut stream = app.docker.logs(
        id,
        Some(bollard::query_parameters::LogsOptions { stdout: true, stderr: true, tail: lines.to_string(), ..Default::default() }),
    );
    let mut out = vec![];
    while let Some(Ok(chunk)) = stream.next().await {
        out.extend(String::from_utf8_lossy(&chunk.into_bytes()).lines().map(String::from));
    }
    out
}

/// Detect GPUs once at startup so containers can be given access.
pub async fn detect_gpu(app: &AppState) {
    if let Ok(info) = app.docker.info().await {
        let nvidia = info.runtimes.as_ref().is_some_and(|r| r.contains_key("nvidia"));
        let _ = app.db.set_state("gpu:nvidia", if nvidia { "1" } else { "0" });
    }
}
