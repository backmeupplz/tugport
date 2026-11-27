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

/// A service's turn: waking, putting to sleep and starting take turns. It holds when the
/// current wake-up gives up; once that has passed, waiting requests try once and fail fast
/// instead of each waiting as long again.
type Turn = Arc<tokio::sync::Mutex<Option<Instant>>>;
static TURNS: LazyLock<Mutex<HashMap<String, Turn>>> = LazyLock::new(Default::default);
/// When this process started: services count as busy until then.
static STARTED: LazyLock<i64> = LazyLock::new(util::now);
/// The least time a wake-up waits for the app to answer.
const MIN_WAKE_S: u32 = if cfg!(test) { 1 } else { 30 };

pub fn turn(service_id: &str) -> Turn {
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
    let mut deadline = gate.lock().await;
    wake_in_turn(app, service_id, port, &mut deadline).await
}

/// API stop sets this before it waits for its turn: a wake-up in progress gives way at once.
fn stopped(app: &App, service_id: &str) -> bool {
    app.db.state::<String>(&format!("stopped:{service_id}")).is_some()
}

/// `wake` for a caller holding the service's turn (`deadline`). It waits for the app to answer
/// on `port` only: a service with domains on several ports may answer on the others a moment
/// later. Waiting for all of them would let one port the app never opens stop replicas that
/// serve the rest.
pub async fn wake_in_turn(app: &App, service_id: &str, port: u16, deadline: &mut Option<Instant>) -> Result<Vec<Upstream>> {
    if stopped(app, service_id) {
        bail!("the service is stopped");
    }
    let s = store::service(app, service_id)?.ok_or_else(|| anyhow!("this service no longer exists"))?;
    let reps = runtime::current(app, &s).await?;
    if reps.is_empty() {
        bail!("there is nothing to start; deploy the service first");
    }
    let started = Instant::now();
    let timeout = Duration::from_secs(s.spec.rollout.health_timeout_s.max(MIN_WAKE_S).into());
    // Asleep until every replica answers: routes built meanwhile (the start triggers a rebuild)
    // give it no targets, so requests wait here instead of reaching an app not listening yet.
    let waking = is_asleep(app, &s.id);
    // A stale route of a service stopped meanwhile (API stop clears `asleep`): nothing to wait for.
    if !waking && !reps.iter().any(|r| r.state == "running") {
        bail!("the service is not running");
    }
    if waking && deadline.is_none() {
        tracing::info!("waking {}/{}", s.project, s.name);
        // A replica that fails to start stays quiet, so the others still serve.
        let mut running = reps.iter().filter(|r| r.state == "running").count();
        for r in reps.iter().filter(|r| r.state != "running") {
            match runtime::start(app, &r.node, &r.id).await {
                Ok(()) => running += 1,
                Err(e) => tracing::warn!("waking {}/{}: {e:#}", s.project, s.name),
            }
        }
        if running == 0 {
            bail!("no container of the service could start");
        }
        // Only now: when nothing started, or this was cut short, the next request starts it.
        *deadline = Some(started + timeout);
    }
    let until = deadline.filter(|_| waking).unwrap_or(started + timeout);
    let proxies = crate::nodes::proxy_addresses(app);
    loop {
        // Stopped meanwhile: the stop, waiting for this turn, stops what this started.
        if stopped(app, &s.id) {
            bail!("the service was stopped");
        }
        let (mut up, mut quiet) = (vec![], vec![]);
        for r in runtime::current(app, &s).await? {
            let ip = r.ip.as_deref().and_then(|a| a.parse::<std::net::IpAddr>().ok()).filter(|_| r.state == "running");
            let answers = match ip {
                Some(ip) => runtime::probe(app, &r.node, &ip.to_string(), port, None, false).await,
                None => false,
            };
            let (Some(ip), true) = (ip, answers) else {
                quiet.push(r);
                continue;
            };
            let local = Upstream::Local(SocketAddr::new(ip, port));
            up.push(if r.node == "local" { local } else { proxies.get(&r.node).copied().map(Upstream::Node).unwrap_or(local) });
        }
        let timed_out = Instant::now() >= until;
        if !up.is_empty() && (!waking || quiet.is_empty() || timed_out) {
            if waking {
                // The rebuild routes to every running replica: those that never answered stop.
                let stops = quiet.iter().map(|r| runtime::stop(app, &r.node, &r.id, 5));
                for (r, res) in quiet.iter().zip(futures_util::future::join_all(stops).await) {
                    tracing::warn!("{}/{}: {} did not answer within {}s; stopped it", s.project, s.name, r.name, timeout.as_secs());
                    if let Err(e) = res {
                        tracing::warn!("stopping {}: {e:#}", r.name);
                    }
                }
                app.db.del_state(&format!("asleep:{}", s.id))?;
                *deadline = None;
                mark_active(app, &s.id);
                app.emit("service.status", Some(&s.project_id), Some(&s.id), serde_json::json!({ "event": "wake", "state": "running" }));
            }
            if waking || started.elapsed() > Duration::from_millis(50) {
                // Other servers learn the new routes before their proxies see a forwarded request.
                crate::proxy::rebuild_now(app).await;
                tracing::info!("{}/{} answered after {:.1}s", s.project, s.name, started.elapsed().as_secs_f32());
            }
            return Ok(up);
        }
        // Still asleep; sleep_idle stops what runs, and the request after that wakes it anew.
        if timed_out {
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
        if is_asleep(app, &s.id) {
            stop_leftovers(app, &s).await;
            continue;
        }
        let Some(cfg) = &s.spec.sleep else { continue };
        // Only web services can be woken by a request.
        if s.kind != "app" || crate::nodes::stopped(app, &s.id) || app.deployer.busy(&s.id) || store::domains(app, Some(&s.id))?.is_empty() {
            continue;
        }
        let idle = || {
            let active: i64 = app.db.get_state(&format!("active:{}", s.id)).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
            util::now() - traffic.get(&s.id).map(|t| t.last_at).unwrap_or(0).max(active).max(*STARTED)
        };
        let limit = cfg.idle_minutes.max(1) as i64 * 60;
        if idle() < limit {
            continue;
        }
        let gate = turn(&s.id);
        let mut deadline = gate.lock().await;
        // Started, stopped, deployed or woken while this waited for its turn.
        if crate::nodes::stopped(app, &s.id) || app.deployer.busy(&s.id) || idle() < limit {
            continue;
        }
        *deadline = None;
        // Asleep first, so new requests wait for a wake-up instead of reaching a stopping container.
        app.db.set_state(&format!("asleep:{}", s.id), &util::now().to_string())?;
        crate::proxy::rebuild_now(app).await;
        for r in runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.state == "running") {
            runtime::stop(app, &r.node, &r.id, 30).await?;
        }
        tracing::info!("{}/{} is asleep after {} minutes without requests", s.project, s.name, idle() / 60);
        app.emit("service.status", Some(&s.project_id), Some(&s.id), serde_json::json!({ "event": "sleep", "state": "asleep" }));
    }
    Ok(())
}

/// A wake-up that gave up, or whose requests all went away, leaves an asleep service's
/// containers running: stop them once its time is up (unless a deploy is on), so the next
/// request wakes the service anew.
async fn stop_leftovers(app: &App, s: &store::Service) {
    let gate = turn(&s.id);
    // Behind the requests already waiting (each fails fast once the time is up), not a wake-up.
    let Ok(mut deadline) = tokio::time::timeout(Duration::from_secs(5), gate.lock()).await else { return };
    if deadline.is_some_and(|d| Instant::now() < d) || !is_asleep(app, &s.id) || app.deployer.busy(&s.id) {
        return;
    }
    let Ok(listing) = runtime::known_replicas(app).await else { return };
    for r in listing.get(&s.id).into_iter().flatten().filter(|r| r.state == "running") {
        tracing::info!("{}/{} is asleep: stopping {}", s.project, s.name, r.name);
        if let Err(e) = runtime::stop(app, &r.node, &r.id, 30).await {
            tracing::warn!("stopping {}: {e:#}", r.name);
            return;
        }
    }
    *deadline = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panel whose Docker is a fake listing one stopped container per IP in `ips` (c0, c1, ...)
    /// of an asleep service (returned; domain a.test on `port`, health timeout `timeout_s`). The
    /// fake records the actions asked of it, as "start c0".
    async fn asleep_app(port: u16, timeout_s: u32, ips: &'static [&'static str]) -> (AppState, String, Arc<Mutex<Vec<String>>>, std::path::PathBuf) {
        let (app, id, actions, dir, _) = asleep_app_held(port, timeout_s, ips).await;
        (app, id, actions, dir)
    }

    /// `asleep_app` whose fake Docker finishes a start only while the returned lock is not
    /// held for writing (it records the start on arrival).
    async fn asleep_app_held(port: u16, timeout_s: u32, ips: &'static [&'static str]) -> (AppState, String, Arc<Mutex<Vec<String>>>, std::path::PathBuf, Arc<tokio::sync::RwLock<()>>) {
        let dir = std::env::temp_dir().join(format!("tugport-test-{}", util::id()));
        let paths = crate::paths::Paths { etc: dir.join("etc"), data: dir.join("data"), run: dir.join("run") };
        std::fs::create_dir_all(&paths.data).unwrap();
        let db = crate::db::Db::open(&paths.db()).unwrap();
        // Its own id: turns are taken per service id across the whole process.
        let id = util::id();
        let spec = serde_json::json!({ "rollout": { "health_timeout_s": timeout_s }, "sleep": { "idle_minutes": 5 } }).to_string();
        db.exec("INSERT INTO projects (id, name, created_at) VALUES ('p', 'shop', 0)", []).unwrap();
        db.exec("INSERT INTO services (id, project_id, name, kind, spec, created_at, updated_at) VALUES (?1, 'p', 'web', 'app', ?2, 0, 0)", [&id, &spec]).unwrap();
        db.exec("INSERT INTO domains (id, service_id, host, port, created_at) VALUES ('d', ?1, 'a.test', ?2, 0)", rusqlite::params![id, port]).unwrap();
        db.set_state(&format!("asleep:{id}"), "0").unwrap();
        let actions: Arc<Mutex<Vec<String>>> = Default::default();
        let (log, sid) = (actions.clone(), id.clone());
        let running: Arc<Mutex<std::collections::HashSet<String>>> = Default::default();
        let hold: Arc<tokio::sync::RwLock<()>> = Default::default();
        let held = hold.clone();
        let docker_api = axum::Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
            let (log, running, sid, held) = (log.clone(), running.clone(), sid.clone(), held.clone());
            async move {
                if method == axum::http::Method::POST {
                    // /containers/{id}/{action}
                    let mut parts = uri.path().rsplit('/');
                    let (action, c) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default().to_string());
                    util::lock(&log).push(format!("{action} {c}"));
                    if action == "start" || action == "restart" {
                        let _ = held.read().await;
                        util::lock(&running).insert(c.clone());
                    } else {
                        util::lock(&running).remove(&c);
                    }
                    return axum::http::StatusCode::NO_CONTENT.into_response();
                }
                let running = util::lock(&running).clone();
                let containers: Vec<_> = ips
                    .iter()
                    .enumerate()
                    .map(|(i, ip)| {
                        let c = format!("c{i}");
                        serde_json::json!({
                            "Id": c, "Names": [format!("/web-{i}")], "State": if running.contains(&c) { "running" } else { "exited" }, "Created": 0,
                            "Labels": { "tugport.managed": "true", "tugport.service": sid, "tugport.deploy": "d1", "tugport.replica": i.to_string() },
                            "NetworkSettings": { "Networks": { "tp-ingress": { "IPAddress": ip } } },
                        })
                    })
                    .collect();
                axum::Json(containers).into_response()
            }
        });
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, docker_api).await });
        let docker = bollard::Docker::connect_with_http(&format!("http://{addr}"), 5, bollard::API_DEFAULT_VERSION).unwrap();
        let selflog = crate::logs::SelfLog::new(dir.join("self"));
        (App::new(paths, db, crate::crypto::Keyring::new(&[7; 32]), docker, selflog, 1).unwrap(), id, actions, dir, hold)
    }

    /// A port nothing listens on (yet). A socket bound to it without listening would keep
    /// others off it, but connections to such a socket hang on macOS instead of failing.
    async fn free_port() -> u16 {
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
    }

    async fn route_targets(app: &App) -> Vec<crate::proxy::Target> {
        crate::proxy::rebuild_now(app).await;
        app.proxy.table().routes.iter().find(|r| r.host == "a.test").unwrap().targets.clone()
    }

    fn actions_of(actions: &Mutex<Vec<String>>) -> Vec<String> {
        util::lock(actions).clone()
    }

    /// Until the fake Docker is asked for something (failing rather than hanging if it never is).
    async fn first_action(actions: &Mutex<Vec<String>>) {
        let asked = async {
            while actions_of(actions).is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), asked).await.expect("nothing was asked of Docker");
    }

    /// While the woken app is not listening yet, its route has no targets and it shows as asleep.
    #[tokio::test]
    async fn requests_wait_until_the_woken_app_answers() {
        let port = free_port().await;
        let (app, id, actions, dir) = asleep_app(port, 180, &["127.0.0.1"]).await;
        let waking = tokio::spawn({
            let (app, id) = (app.clone(), id.clone());
            async move { wake(&app, &id, port).await }
        });
        first_action(&actions).await;
        // The container runs, the app inside does not listen yet.
        assert_eq!(route_targets(&app).await, vec![]);
        assert!(is_asleep(&app, &id));
        let s = store::service(&app, &id).unwrap().unwrap();
        assert_eq!(runtime::service_status(&app, &s).await["state"], "asleep");
        let _listening = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        assert_eq!(waking.await.unwrap().unwrap(), vec![Upstream::Local(addr)]);
        assert!(!is_asleep(&app, &id));
        assert_eq!(route_targets(&app).await, vec![crate::proxy::Target { node: "local".into(), addr }]);
        assert_eq!(runtime::service_status(&app, &s).await["state"], "running");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A request on a stale route of a service stopped while asleep fails at once, starting nothing.
    #[tokio::test]
    async fn a_service_stopped_while_asleep_is_not_woken() {
        let port = free_port().await;
        let (app, id, actions, dir) = asleep_app(port, 180, &["127.0.0.1"]).await;
        app.db.del_state(&format!("asleep:{id}")).unwrap();
        let t = Instant::now();
        assert!(wake(&app, &id, port).await.is_err());
        assert!(t.elapsed() < Duration::from_millis(500));
        assert!(actions_of(&actions).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A wake-up that times out leaves the service asleep; requests after it fail at once, and
    /// sleep_idle stops what it started so the next request wakes it anew.
    #[tokio::test]
    async fn a_timed_out_wake_goes_back_to_sleep() {
        let port = free_port().await;
        let (app, id, actions, dir) = asleep_app(port, 0, &["127.0.0.1"]).await;
        assert!(wake(&app, &id, port).await.is_err());
        assert!(is_asleep(&app, &id));
        let t = Instant::now();
        assert!(wake(&app, &id, port).await.is_err());
        assert!(t.elapsed() < Duration::from_millis(500));
        assert_eq!(actions_of(&actions), ["start c0"]);
        sleep_idle(app.clone()).await.unwrap();
        assert_eq!(actions_of(&actions), ["start c0", "stop c0"]);
        assert!(is_asleep(&app, &id));
        assert!(wake(&app, &id, port).await.is_err());
        assert_eq!(actions_of(&actions), ["start c0", "stop c0", "start c0"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A wake-up whose request went away keeps its containers until its time is up.
    #[tokio::test]
    async fn a_dropped_wake_keeps_booting() {
        let port = free_port().await;
        let (app, id, actions, dir) = asleep_app(port, 180, &["127.0.0.1"]).await;
        let waking = tokio::spawn({
            let (app, id) = (app.clone(), id.clone());
            async move { wake(&app, &id, port).await }
        });
        first_action(&actions).await;
        // Past the start, probing.
        tokio::time::sleep(Duration::from_millis(300)).await;
        waking.abort();
        let _ = waking.await;
        sleep_idle(app.clone()).await.unwrap();
        assert_eq!(actions_of(&actions), ["start c0"]);
        *turn(&id).try_lock().unwrap() = Some(Instant::now());
        sleep_idle(app.clone()).await.unwrap();
        assert_eq!(actions_of(&actions), ["start c0", "stop c0"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Replicas answer before any gets requests; one still quiet at the timeout is stopped.
    #[tokio::test]
    async fn a_quiet_replica_is_stopped_at_the_timeout() {
        if std::net::TcpListener::bind("[::1]:0").is_err() {
            return; // no IPv6 loopback to tell the replicas apart
        }
        let port = free_port().await;
        let (app, id, actions, dir) = asleep_app(port, 0, &["127.0.0.1", "::1"]).await;
        let _listening = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let t = Instant::now();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        assert_eq!(wake(&app, &id, port).await.unwrap(), vec![Upstream::Local(addr)]);
        assert!(t.elapsed() >= Duration::from_secs(1));
        assert_eq!(actions_of(&actions), ["start c0", "start c1", "stop c1"]);
        assert!(!is_asleep(&app, &id));
        assert_eq!(route_targets(&app).await, vec![crate::proxy::Target { node: "local".into(), addr }]);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn owner() -> crate::api::Auth {
        crate::api::Auth(crate::auth::Principal {
            user_id: "u".into(),
            name: "test".into(),
            kind: crate::auth::UserKind::Human,
            is_owner: true,
            all_projects_role: None,
            project_roles: Default::default(),
            grants: vec![],
            key_id: None,
            ip: None,
        })
    }

    /// API stop while a wake-up is still starting the containers: the wake-up gives way and the
    /// stop stops what it started, so the service ends stopped, not running with `stopped:` set.
    #[tokio::test]
    async fn a_stop_during_a_wake_up_stops_what_it_started() {
        use axum::extract::{Path, State};
        let port = free_port().await;
        let (app, id, actions, dir, hold) = asleep_app_held(port, 180, &["127.0.0.1"]).await;
        let held = hold.clone().write_owned().await;
        let waking = tokio::spawn({
            let (app, id) = (app.clone(), id.clone());
            async move { wake(&app, &id, port).await }
        });
        first_action(&actions).await;
        // The start is on its way to Docker: a stop that did not wait its turn listed the replicas
        // meanwhile and stopped nothing.
        let stopping = tokio::spawn(crate::api::deploys::stop(State(app.clone()), owner(), Path(id.clone())));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _listening = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        drop(held);
        let t = Instant::now();
        assert!(waking.await.unwrap().is_err());
        let _ = stopping.await.unwrap().unwrap();
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(runtime::replicas(&app, &id).await.unwrap().iter().all(|r| r.state != "running"));
        assert!(stopped(&app, &id));
        assert!(!is_asleep(&app, &id));
        assert_eq!(route_targets(&app).await, vec![]);
        // A request on a stale route starts nothing.
        assert!(wake(&app, &id, port).await.is_err());
        assert_eq!(actions_of(&actions), ["start c0", "stop c0"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// API start or restart of a sleeping service wakes it: requests wait until the app answers.
    /// A restart also restarts a container an earlier wake-up that gave up left running.
    async fn waking_through_the_api(restart: bool) {
        use axum::extract::{Path, State};
        let port = free_port().await;
        let (app, id, actions, dir) = asleep_app(port, 180, &["127.0.0.1"]).await;
        if restart {
            runtime::start(&app, "local", "c0").await.unwrap();
            util::lock(&actions).clear();
        }
        let (st, a, path) = (State(app.clone()), owner(), Path(id.clone()));
        let starting = tokio::spawn(async move {
            if restart { crate::api::deploys::restart(st, a, path).await } else { crate::api::deploys::start(st, a, path).await }
        });
        first_action(&actions).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(route_targets(&app).await, vec![]);
        assert!(is_asleep(&app, &id));
        let _listening = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let _ = starting.await.unwrap().unwrap();
        assert!(!is_asleep(&app, &id));
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        assert_eq!(route_targets(&app).await, vec![crate::proxy::Target { node: "local".into(), addr }]);
        assert_eq!(actions_of(&actions), [if restart { "restart c0" } else { "start c0" }]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A start or restart of a stopped service keeps it marked stopped until its containers have
    /// started, then restarts the idle clock so sleep_idle leaves it running.
    async fn unmarking_stopped(restart: bool) {
        use axum::extract::{Path, State};
        let port = free_port().await;
        let (app, id, actions, dir, hold) = asleep_app_held(port, 180, &["127.0.0.1"]).await;
        app.db.del_state(&format!("asleep:{id}")).unwrap();
        app.db.set_state(&format!("stopped:{id}"), "1").unwrap();
        let held = hold.clone().write_owned().await;
        let (st, a, path) = (State(app.clone()), owner(), Path(id.clone()));
        let starting = tokio::spawn(async move {
            if restart { crate::api::deploys::restart(st, a, path).await } else { crate::api::deploys::start(st, a, path).await }
        });
        first_action(&actions).await;
        assert!(stopped(&app, &id));
        drop(held);
        let _ = starting.await.unwrap().unwrap();
        assert!(!stopped(&app, &id));
        assert!(app.db.state::<String>(&format!("active:{id}")).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_start_unmarks_stopped_only_once_started() {
        unmarking_stopped(false).await;
    }

    #[tokio::test]
    async fn a_restart_unmarks_stopped_only_once_started() {
        unmarking_stopped(true).await;
    }

    #[tokio::test]
    async fn starting_a_sleeping_service_waits_until_it_answers() {
        waking_through_the_api(false).await;
    }

    #[tokio::test]
    async fn restarting_a_sleeping_service_waits_until_it_answers() {
        waking_through_the_api(true).await;
    }

    /// A start that gets its turn before a stop (both behind a wake-up) does not undo the stop.
    #[tokio::test]
    async fn a_stop_behind_a_start_still_stops() {
        use axum::extract::{Path, State};
        let port = free_port().await;
        let (app, id, actions, dir, hold) = asleep_app_held(port, 180, &["127.0.0.1"]).await;
        let held = hold.clone().write_owned().await;
        let waking = tokio::spawn({
            let (app, id) = (app.clone(), id.clone());
            async move { wake(&app, &id, port).await }
        });
        first_action(&actions).await;
        // Turns are handed out in order: the start, then the stop.
        let starting = tokio::spawn(crate::api::deploys::start(State(app.clone()), owner(), Path(id.clone())));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stopping = tokio::spawn(crate::api::deploys::stop(State(app.clone()), owner(), Path(id.clone())));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _listening = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        drop(held);
        assert!(waking.await.unwrap().is_err());
        let _ = starting.await.unwrap().unwrap();
        let _ = stopping.await.unwrap().unwrap();
        assert!(runtime::replicas(&app, &id).await.unwrap().iter().all(|r| r.state != "running"));
        assert!(stopped(&app, &id));
        assert!(!is_asleep(&app, &id));
        let _ = std::fs::remove_dir_all(dir);
    }
}
