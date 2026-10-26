//! The built-in reverse proxy: host/path routing to live replicas, HTTPS with automatic
//! certificates, WebSockets, gRPC, redirects, auth and maintenance pages.

use crate::app::{App, AppState};
use crate::model::{DomainOptions, WwwRedirect};
use crate::{runtime, store};
use serde::{Deserialize, Serialize};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;
pub type Body = BoxBody<Bytes, BoxErr>;

/// The routing table as data: built by the panel, pushed to every server, and compiled
/// there into `Routes` relative to that server.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub routes: Vec<RouteSpec>,
    /// Node id -> that node's proxy on the mesh.
    pub nodes: HashMap<String, SocketAddr>,
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// This server is not an ingress: it serves only requests forwarded from one (the hop header),
    /// and its routes carry no auth config, so it must refuse direct public requests.
    #[serde(default)]
    pub worker: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouteSpec {
    pub host: String,
    pub path: String,
    pub service_id: Option<String>,
    pub project_id: Option<String>,
    pub https: bool,
    pub via_tunnel: bool,
    pub options: DomainOptions,
    pub targets: Vec<Target>,
    pub asleep: bool,
    /// The container port requests go to (to wake a sleeping service).
    #[serde(default)]
    pub port: u16,
}

/// A replica: the node it runs on and its address as seen from that node.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Target {
    pub node: String,
    pub addr: SocketAddr,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Upstream {
    /// A replica on this server.
    Local(SocketAddr),
    /// Another server's proxy, which serves its own replicas.
    Node(SocketAddr),
}

/// Requests one proxy forwards to another carry this header.
const HOP: &str = "x-tugport-hop";

struct Route {
    port: u16,
    service_id: Option<String>,
    path: String,
    upstreams: Vec<Upstream>,
    https: bool,
    via_tunnel: bool,
    options: DomainOptions,
    redirects: Vec<(regex::Regex, String, bool)>,
    rewrite: Option<(regex::Regex, String)>,
    /// Service is scaled to zero; the first request wakes it.
    asleep: bool,
}

#[derive(Default)]
struct Routes {
    hosts: HashMap<String, Vec<Arc<Route>>>,
    trusted: Vec<ipnet::IpNet>,
    worker: bool,
}

/// What a non-ingress server is given: only the routes it hosts a replica of, with its own
/// targets and without the secrets it never uses. On a request forwarded from an ingress server
/// (the hop header) such a server skips auth and counts no traffic, so it needs neither the
/// basic-auth hashes nor the forward-auth settings, nor other servers' addresses or certificates.
pub fn reduce_for_worker(table: &Table, node: &str) -> Table {
    let routes = table
        .routes
        .iter()
        .filter(|r| r.targets.iter().any(|t| t.node == node))
        .map(|r| {
            let mut options = r.options.clone();
            options.basic_auth = vec![];
            options.forward_auth = None;
            RouteSpec {
                options,
                targets: r.targets.iter().filter(|t| t.node == node).cloned().collect(),
                ..r.clone()
            }
        })
        .collect();
    Table { routes, nodes: HashMap::new(), trusted_proxies: vec![], worker: true }
}

/// Turn a table into routes as seen from node `me`.
fn compile(table: &Table, me: &str) -> Routes {
    let mut hosts: HashMap<String, Vec<Arc<Route>>> = HashMap::new();
    for spec in &table.routes {
        let mut upstreams: Vec<Upstream> = vec![];
        for t in &spec.targets {
            let u = if t.node == me { Some(Upstream::Local(t.addr)) } else { table.nodes.get(&t.node).map(|a| Upstream::Node(*a)) };
            if let Some(u) = u
                && !upstreams.contains(&u)
            {
                upstreams.push(u);
            }
        }
        let redirects = spec.options.redirects.iter().filter_map(|r| regex::Regex::new(&r.from).ok().map(|re| (re, r.to.clone(), r.permanent))).collect();
        let rewrite = spec.options.rewrite.as_ref().and_then(|r| regex::Regex::new(&r.from).ok().map(|re| (re, r.to.clone())));
        // Only the panel can wake a sleeping service; other servers send such requests there.
        if spec.asleep
            && upstreams.is_empty()
            && me != "local"
            && let Some(panel) = table.nodes.get("local")
        {
            upstreams.push(Upstream::Node(*panel));
        }
        hosts.entry(spec.host.clone()).or_default().push(Arc::new(Route {
            port: spec.port,
            service_id: spec.service_id.clone(),
            path: spec.path.clone(),
            upstreams,
            https: spec.https,
            via_tunnel: spec.via_tunnel,
            options: spec.options.clone(),
            redirects,
            rewrite,
            asleep: spec.asleep,
        }));
    }
    for routes in hosts.values_mut() {
        routes.sort_by_key(|r| std::cmp::Reverse(r.path.len()));
    }
    let mut trusted = table.trusted_proxies.clone();
    if let Ok(mesh) = crate::mesh::SUBNET.parse() {
        trusted.push(mesh);
    }
    Routes { hosts, trusted, worker: table.worker }
}

impl Routes {
    fn find(&self, host: &str, path: &str) -> Option<Arc<Route>> {
        self.hosts.get(host)?.iter().find(|r| path_under(path, &r.path)).cloned()
    }
}

fn path_under(path: &str, prefix: &str) -> bool {
    prefix == "/" || path == prefix || path.strip_prefix(prefix.trim_end_matches('/')).is_some_and(|rest| rest.starts_with('/'))
}

#[derive(Default, Clone, Copy, Serialize, Deserialize)]
pub struct Traffic {
    pub requests: u64,
    pub last_at: i64,
}

pub struct Proxy {
    routes: RwLock<Arc<Routes>>,
    table: RwLock<Arc<Table>>,
    /// This server's node id ("local" on the panel's server).
    pub me: RwLock<String>,
    certs: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    fallback: Arc<CertifiedKey>,
    pub challenges: Mutex<HashMap<String, String>>,
    rr: AtomicUsize,
    /// Whether this proxy holds its ports; another proxy may still have them during a move.
    pub listening: AtomicBool,
    /// Other servers' proxies that just refused a connection, until when to skip them.
    down: Mutex<HashMap<SocketAddr, std::time::Instant>>,
    /// Containers on their way out (scaling down): no new requests.
    pub draining: Mutex<std::collections::HashSet<String>>,
    /// What other servers' proxies counted, per server (reported with their health).
    pub remote_traffic: Mutex<HashMap<String, HashMap<String, Traffic>>>,
    pub traffic: Mutex<HashMap<String, Traffic>>,
    reload: tokio::sync::Notify,
    http1: Client<HttpConnector, Body>,
    h2c: Client<HttpConnector, Body>,
    auth_cache: Mutex<HashMap<String, i64>>,
    /// Every service's containers from the last table build, which the API reads instead of
    /// asking Docker again (a listing takes ~100 ms on a busy server).
    pub replicas: RwLock<Option<runtime::Listing>>,
}

impl Proxy {
    pub fn new() -> Result<Self> {
        let mut conn = HttpConnector::new();
        conn.set_connect_timeout(Some(Duration::from_secs(5)));
        conn.set_nodelay(true);
        let self_signed = rcgen::generate_simple_self_signed(vec!["tugport.invalid".into()])?;
        let fallback = certified_key(&self_signed.cert.pem(), &self_signed.signing_key.serialize_pem())?;
        Ok(Proxy {
            routes: RwLock::new(Arc::new(Routes::default())),
            table: RwLock::new(Arc::new(Table::default())),
            me: RwLock::new("local".into()),
            certs: RwLock::new(HashMap::new()),
            fallback: Arc::new(fallback),
            challenges: Mutex::new(HashMap::new()),
            rr: AtomicUsize::new(0),
            listening: AtomicBool::new(false),
            down: Mutex::new(HashMap::new()),
            draining: Mutex::new(Default::default()),
            remote_traffic: Mutex::new(HashMap::new()),
            traffic: Mutex::new(HashMap::new()),
            reload: tokio::sync::Notify::new(),
            http1: Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(conn.clone()),
            h2c: Client::builder(TokioExecutor::new()).http2_only(true).build(conn),
            auth_cache: Mutex::new(HashMap::new()),
            replicas: RwLock::new(None),
        })
    }

    fn routes(&self) -> Arc<Routes> {
        crate::util::read(&self.routes).clone()
    }

    /// Requests per service on every server: counts added up, the latest request time.
    pub fn all_traffic(&self) -> HashMap<String, Traffic> {
        let mut all = crate::util::lock(&self.traffic).clone();
        for per_node in crate::util::lock(&self.remote_traffic).values() {
            for (sid, t) in per_node {
                let e = all.entry(sid.clone()).or_default();
                e.requests += t.requests;
                e.last_at = e.last_at.max(t.last_at);
            }
        }
        all
    }

    pub fn table(&self) -> Arc<Table> {
        crate::util::read(&self.table).clone()
    }

    /// Install a routing table (the panel builds it; servers receive it over the mesh).
    pub fn set_table(&self, table: Table) {
        let me = crate::util::read(&self.me).clone();
        *crate::util::write(&self.routes) = Arc::new(compile(&table, &me));
        *crate::util::write(&self.table) = Arc::new(table);
    }

    pub fn set_cert(&self, host: &str, key: CertifiedKey) {
        crate::util::write(&self.certs).insert(host.to_string(), Arc::new(key));
    }

    pub fn has_cert(&self, host: &str) -> bool {
        crate::util::read(&self.certs).contains_key(host)
    }
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Proxy")
    }
}

struct Resolver(AppState);

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolver")
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let p = &self.0.proxy;
        let certs = crate::util::read(&p.certs);
        hello.server_name().and_then(|n| certs.get(&n.to_ascii_lowercase()).cloned()).or_else(|| Some(p.fallback.clone()))
    }
}

pub fn certified_key(cert_pem: &str, key_pem: &str) -> Result<CertifiedKey> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes()).collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)?;
    Ok(CertifiedKey::new(certs, signing))
}

/// Ask for a routing-table rebuild (debounced).
pub fn reload(app: &App) {
    app.proxy.reload.notify_one();
}

async fn build_table(app: &App) -> Result<Table> {
    let mut routes = vec![];
    let services: HashMap<String, store::Service> = store::services(app, None)?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let replicas = match runtime::all_replicas(app).await {
        Ok(r) => {
            let r = Arc::new(r);
            *crate::util::write(&app.proxy.replicas) = Some(r.clone());
            r
        }
        Err(_) => Default::default(),
    };
    let draining = crate::util::lock(&app.proxy.draining).clone();
    let settings = app.settings();
    for d in store::domains(app, None)? {
        // The panel's domain belongs to the panel: a service route there (even a longer path)
        // would receive its requests and the session cookie.
        if settings.panel_domain.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(&d.host)) {
            continue;
        }
        let Some(s) = services.get(&d.service_id) else { continue };
        let live = runtime::live_deploy(app, &s.id);
        let asleep = crate::sleep::is_asleep(app, &s.id);
        let targets = replicas
            .get(&s.id)
            .into_iter()
            .flatten()
            // A sleeping service's requests wait for its wake-up, even while it is still stopping.
            .filter(|r| r.state == "running" && !asleep && !draining.contains(&r.id))
            .filter(|r| match (&d.container, s.kind.as_str()) {
                (Some(c), "compose") => r.compose_service.as_deref() == Some(c.as_str()),
                (None, "compose") => true,
                _ => live.as_deref().is_none_or(|l| r.deploy == l),
            })
            .filter_map(|r| Some(Target { node: r.node.clone(), addr: SocketAddr::new(r.ip.as_ref()?.parse().ok()?, d.port) }))
            .collect();
        routes.push(RouteSpec {
            host: d.host.clone(),
            path: d.path.clone(),
            service_id: Some(s.id.clone()),
            project_id: Some(s.project_id.clone()),
            https: d.https,
            via_tunnel: d.via_tunnel,
            options: d.options.clone(),
            targets,
            asleep,
            port: d.port,
        });
    }
    if let Some(panel) = &settings.panel_domain {
        let port = settings.panel_listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(8484);
        let ip = settings.panel_listen.rsplit_once(':').and_then(|(ip, _)| ip.trim_matches(['[', ']']).parse::<IpAddr>().ok()).filter(|ip| !ip.is_unspecified());
        let addr = SocketAddr::new(ip.unwrap_or(IpAddr::from([127, 0, 0, 1])), port);
        // A private panel still takes webhooks on its public domain, and nothing else.
        let paths: &[&str] = if settings.panel_public { &["/"] } else { &["/api/hooks", "/api/deploy", "/api/nodes/join"] };
        for path in paths {
            routes.push(RouteSpec {
                host: panel.clone(),
                path: path.to_string(),
                service_id: None,
                project_id: None,
                https: true,
                via_tunnel: false,
                options: DomainOptions { timeout_s: Some(3600), ..Default::default() },
                targets: vec![Target { node: "local".into(), addr }],
                asleep: false,
                port,
            });
        }
    }
    Ok(Table { routes, nodes: crate::nodes::proxy_addresses(app), trusted_proxies: settings.trusted_proxies, worker: false })
}

/// Build and install the routing table; other servers get it when it changed. With `wait`,
/// they get it anyway and this returns once they have it.
async fn rebuild(app: &App, wait: bool) {
    let t = match build_table(app).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("building routes: {e:#}");
            return;
        }
    };
    let changed = *app.proxy.table() != t;
    app.proxy.set_table(t);
    if wait {
        crate::nodes::push_routes_now(app).await;
    } else if changed {
        crate::nodes::push_routes(app);
    }
}

/// Rebuild now and wait until every server has the new table (a woken service's
/// requests may be forwarded to another server right away).
pub async fn rebuild_now(app: &App) {
    rebuild(app, true).await;
}

/// Build the routing table once; call before serving so the first request already routes.
pub async fn initial_routes(app: &App) {
    rebuild(app, false).await;
}

/// Keep the routing table (and the API's container listing) fresh: on demand, on container
/// events on any server, and every 30 seconds.
pub async fn watch(app: AppState) {
    tokio::spawn(crate::nodes::for_each_node(app.clone(), |app, node| async move {
        use futures_util::StreamExt;
        let filters = HashMap::from([("type".to_string(), vec!["container".to_string()])]);
        let mut events = node.docker.events(Some(bollard::query_parameters::EventsOptions { filters: Some(filters), ..Default::default() }));
        while let Some(Ok(e)) = events.next().await {
            if matches!(e.action.as_deref(), Some("create" | "start" | "die" | "stop" | "pause" | "unpause" | "destroy")) || e.action.as_deref().is_some_and(|a| a.starts_with("health_status")) {
                reload(&app);
            }
        }
    }));
    loop {
        rebuild(&app, false).await;
        tokio::select! {
            _ = app.proxy.reload.notified() => tokio::time::sleep(Duration::from_millis(150)).await,
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
    }
}

pub async fn serve(app: AppState, http_port: u16, https_port: u16) -> Result<()> {
    let tls = {
        let mut cfg = rustls::ServerConfig::builder().with_no_client_auth().with_cert_resolver(Arc::new(Resolver(app.clone())));
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        tokio_rustls::TlsAcceptor::from(Arc::new(cfg))
    };
    let (http_key, https_key) = (format!("proxy:{http_port}"), format!("proxy:{https_port}"));
    let (http, https) = tokio::try_join!(crate::handoff::listener(&http_key, bind(http_port)), crate::handoff::listener(&https_key, bind(https_port)))?;
    tracing::info!("proxy listening on :{http_port} and :{https_port}");
    app.proxy.listening.store(true, Ordering::Relaxed);
    let a = app.clone();
    tokio::spawn(accept(http, move |stream, peer| {
        tokio::spawn(serve_conn(a.clone(), TokioIo::new(stream), peer, false));
    }));
    accept(https, |stream, peer| {
        let (app, tls) = (app.clone(), tls.clone());
        tokio::spawn(async move {
            if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await {
                serve_conn(app, TokioIo::new(s), peer, true).await
            }
        });
    })
    .await;
    Ok(())
}

/// Hand each connection to `on` until the process hands its listeners over.
async fn accept(listener: tokio::net::TcpListener, on: impl Fn(tokio::net::TcpStream, SocketAddr)) {
    while !crate::handoff::draining() {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = crate::handoff::DRAIN.notified() => break,
        };
        match accepted {
            Ok((stream, peer)) => on(stream, peer),
            // Out of file descriptors, say: back off instead of spinning.
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Bind, waiting while another proxy (Traefik during a migration) still holds the port.
async fn bind(port: u16) -> Result<tokio::net::TcpListener> {
    let in_use = |e: &std::io::Error| e.kind() == std::io::ErrorKind::AddrInUse;
    let mut warned = false;
    loop {
        // IPv6 (dual-stack) first; hosts without IPv6 get IPv4 only.
        let bound = match tokio::net::TcpListener::bind(("::", port)).await {
            Err(e) if !in_use(&e) => tokio::net::TcpListener::bind(("0.0.0.0", port)).await,
            r => r,
        };
        match bound {
            Ok(l) => return Ok(l),
            Err(e) if !in_use(&e) => return Err(e.into()),
            Err(_) => {}
        }
        if !warned {
            tracing::warn!("port {port} is in use; the proxy will take it over as soon as it is free");
            warned = true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn serve_conn<I>(app: AppState, io: TokioIo<I>, peer: SocketAddr, tls: bool)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let _guard = crate::handoff::ConnGuard::new();
    // Dual-stack listeners see IPv4 clients as ::ffff:a.b.c.d; trust checks need a.b.c.d.
    let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
    let svc = hyper::service::service_fn(move |req| {
        let app = app.clone();
        async move { Ok::<_, std::convert::Infallible>(handle(app, req, peer, tls).await) }
    });
    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    let conn = builder.serve_connection_with_upgrades(io, svc);
    tokio::pin!(conn);
    tokio::select! {
        _ = conn.as_mut() => {}
        _ = crate::handoff::DRAIN.notified() => {
            // Finish the requests in flight, then close (keep-alive ends here).
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

fn full_body(data: impl Into<Bytes>) -> Body {
    Full::new(data.into()).map_err(|e| match e {}).boxed()
}

fn full(status: StatusCode, content_type: &'static str, data: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(full_body(data));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn redirect(status: StatusCode, to: &str) -> Response<Body> {
    let mut r = full(status, "text/plain", format!("Redirecting to {to}\n"));
    r.headers_mut().insert(header::LOCATION, HeaderValue::from_str(to).unwrap_or(HeaderValue::from_static("/")));
    r
}

pub fn page(status: StatusCode, title: &str, text: &str) -> Response<Body> {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let (title, text) = (esc(title), esc(text));
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{title}</title>\
         <style>body{{font:16px/1.6 system-ui,sans-serif;background:#0b1018;color:#e9eef5;display:grid;place-items:center;min-height:100vh;margin:0}}\
         main{{max-width:30em;padding:24px}}h1{{font-size:22px;margin:0 0 8px}}p{{color:#a3afc0;margin:0}}</style>\
         <main><h1>{title}</h1><p>{text}</p></main>"
    );
    html_page(status, html)
}

fn html_page(status: StatusCode, html: impl Into<Bytes>) -> Response<Body> {
    full(status, "text/html; charset=utf-8", html)
}

fn error_page(route: &Route, status: StatusCode, title: &str, text: &str) -> Response<Body> {
    match route.options.error_pages.get(&status.as_u16()) {
        Some(custom) => html_page(status, custom.clone()),
        None => page(status, title, text),
    }
}

/// The requested host, lowercase, without port, brackets or a trailing dot.
fn host_of<B>(req: &Request<B>) -> String {
    let raw = req.uri().authority().map(|a| a.as_str()).or_else(|| req.headers().get(header::HOST).and_then(|h| h.to_str().ok())).unwrap_or_default();
    let host = match raw.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => raw.rsplit_once(':').filter(|(_, port)| port.bytes().all(|c| c.is_ascii_digit())).map_or(raw, |(h, _)| h),
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// Address of the real client, honouring X-Forwarded-For only from trusted proxies.
fn client_ip(headers: &HeaderMap, peer: IpAddr, trusted: &[ipnet::IpNet]) -> (IpAddr, bool) {
    let is_trusted = |ip: &IpAddr| ip.is_loopback() || trusted.iter().any(|n| n.contains(ip));
    if !is_trusted(&peer) {
        return (peer, false);
    }
    let chain: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .collect();
    let client = chain.iter().rev().find(|ip| !is_trusted(ip)).copied().or(chain.first().copied()).unwrap_or(peer);
    (client, true)
}

async fn handle(app: AppState, mut req: Request<Incoming>, peer: SocketAddr, tls: bool) -> Response<Body> {
    let host = host_of(&req);
    let path = req.uri().path();

    if !tls && let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
        return match acme_answer(&app, &host, path, token).await {
            Some(key_auth) => full(StatusCode::OK, "text/plain", key_auth),
            None => full(StatusCode::NOT_FOUND, "text/plain", "unknown challenge"),
        };
    }

    let routes = app.proxy.routes();
    let Some(route) = routes.find(&host, path) else {
        return page(StatusCode::NOT_FOUND, "Nothing here yet", &format!("No service is set up for {host}."));
    };
    let (client, via_trusted) = client_ip(req.headers(), peer.ip(), &routes.trusted);
    // A neighbour proxy already chose this server and checked auth: serve from local
    // replicas only. Only Tugport itself can open connections from the mesh.
    let hop = req.headers().contains_key(HOP) && in_mesh(peer.ip());
    // A non-ingress server has no certificates or auth config; it serves only forwarded
    // (hop) requests. A direct public request means DNS points at it by mistake, and serving
    // it would skip the site's basic auth, so refuse.
    if routes.worker && !hop && route.service_id.is_some() {
        return error_page(&route, StatusCode::SERVICE_UNAVAILABLE, "Not an entry point", "This server does not serve this site directly; point its DNS at the panel or mark this server as an ingress.");
    }
    let forwarded_https = via_trusted && req.headers().get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let scheme = if tls || forwarded_https { "https" } else { "http" };
    let path_and_query = req.uri().path_and_query().map_or("/", |p| p.as_str());

    if route.https && scheme == "http" && !route.via_tunnel && app.proxy.has_cert(&host) {
        return redirect(StatusCode::PERMANENT_REDIRECT, &format!("https://{host}{path_and_query}"));
    }
    if let Some(html) = &route.options.maintenance {
        return html_page(StatusCode::SERVICE_UNAVAILABLE, html.clone());
    }
    match (&route.options.www_redirect, host.strip_prefix("www.")) {
        (Some(WwwRedirect::ToWww), None) => return redirect(StatusCode::MOVED_PERMANENTLY, &format!("{scheme}://www.{host}{path_and_query}")),
        (Some(WwwRedirect::ToApex), Some(apex)) => return redirect(StatusCode::MOVED_PERMANENTLY, &format!("{scheme}://{apex}{path_and_query}")),
        _ => {}
    }
    if !route.redirects.is_empty() {
        let full_url = format!("{scheme}://{host}{path_and_query}");
        if let Some((re, to, permanent)) = route.redirects.iter().find(|(re, _, _)| re.is_match(&full_url)) {
            return redirect(if *permanent { StatusCode::PERMANENT_REDIRECT } else { StatusCode::FOUND }, &re.replace(&full_url, to.as_str()));
        }
    }
    if !hop && !route.options.basic_auth.is_empty() && !basic_auth_ok(&app, &route, req.headers()).await {
        let mut r = page(StatusCode::UNAUTHORIZED, "Sign in", "This site needs a user name and password.");
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Restricted\", charset=\"UTF-8\""));
        return r;
    }
    let mut extra_headers = vec![];
    if let Some(fa) = route.options.forward_auth.as_ref().filter(|_| !hop) {
        match forward_auth(fa, &req, scheme, &host, client).await {
            Ok(h) => extra_headers = h,
            Err(resp) => return resp,
        }
    }

    // Counted once, by the first proxy (scale-to-zero and autoscaling read these).
    if !hop && let Some(sid) = &route.service_id {
        let now = crate::util::now();
        let mut t = crate::util::lock(&app.proxy.traffic);
        match t.get_mut(sid) {
            Some(e) => {
                e.requests += 1;
                e.last_at = now;
            }
            None => {
                t.insert(sid.clone(), Traffic { requests: 1, last_at: now });
            }
        }
    }
    let (upstream, via_node) = match choose_upstream(&app, &route, hop, &host).await {
        Ok(Upstream::Local(a)) => (a, false),
        Ok(Upstream::Node(a)) => (a, true),
        Err(resp) => return resp,
    };
    // Stripping the prefix and rewriting are left to the last proxy.
    let new_pq = if via_node { path_and_query.into() } else { upstream_path(&route, path_and_query) };
    let Ok(uri) = Uri::builder().scheme("http").authority(upstream.to_string()).path_and_query(new_pq.as_ref()).build() else {
        return page(StatusCode::BAD_REQUEST, "Bad request", "The address is not valid.");
    };

    let grpc = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/grpc"));
    let upgrade = req.headers().contains_key(header::UPGRADE);
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));

    let (mut parts, body) = req.into_parts();
    parts.uri = uri;
    strip_hop_headers(&mut parts.headers, upgrade);
    let xff = match parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).filter(|_| via_trusted) {
        Some(prev) => format!("{prev}, {}", peer.ip()),
        None => client.to_string(),
    };
    let host_value = HeaderValue::from_str(&host).ok();
    for (k, v) in [
        (header::HOST, host_value.clone()),
        (HeaderName::from_static("x-forwarded-host"), host_value),
        (HeaderName::from_static("x-forwarded-for"), HeaderValue::from_str(&xff).ok()),
        (HeaderName::from_static("x-real-ip"), HeaderValue::from_str(&client.to_string()).ok()),
        (HeaderName::from_static("x-forwarded-proto"), Some(HeaderValue::from_static(scheme))),
    ] {
        if let Some(v) = v {
            parts.headers.insert(k, v);
        }
    }
    for (k, v) in extra_headers {
        parts.headers.insert(k, v);
    }
    if via_node {
        parts.headers.insert(HeaderName::from_static(HOP), HeaderValue::from_static("1"));
    }
    let body: Body = match route.options.max_body_mb {
        Some(mb) => Limited::new(body, usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)).boxed(),
        None => body.map_err(BoxErr::from).boxed(),
    };
    let client = if grpc && parts.version == Version::HTTP_2 {
        &app.proxy.h2c
    } else {
        parts.version = Version::HTTP_11;
        &app.proxy.http1
    };
    let timeout = Duration::from_secs(route.options.timeout_s.unwrap_or(300));
    let mut resp = match tokio::time::timeout(timeout, client.request(Request::from_parts(parts, body))).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!("upstream {upstream} for {host}: {e}");
            if via_node && e.is_connect() {
                crate::util::lock(&app.proxy.down).insert(upstream, std::time::Instant::now() + Duration::from_secs(15));
                reload(&app);
            }
            return error_page(&route, StatusCode::BAD_GATEWAY, "Bad gateway", "The service did not answer. It may be starting or crashing.");
        }
        Err(_) => return error_page(&route, StatusCode::GATEWAY_TIMEOUT, "Gateway timeout", "The service took too long to answer."),
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS && let Some(client_upgrade) = client_upgrade {
        let upstream_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(async move {
            if let (Ok(c), Ok(u)) = (client_upgrade.await, upstream_upgrade.await) {
                let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(u)).await;
            }
        });
        let (mut rparts, _) = resp.into_parts();
        rparts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(rparts, full_body(Bytes::new()));
    }
    let (mut rparts, rbody) = resp.into_parts();
    if (502..=504).contains(&rparts.status.as_u16())
        && let Some(custom) = route.options.error_pages.get(&rparts.status.as_u16())
    {
        return html_page(rparts.status, custom.clone());
    }
    strip_hop_headers(&mut rparts.headers, false);
    Response::from_parts(rparts, rbody.map_err(BoxErr::from).boxed())
}

/// Where a request goes: local replicas only for a hop, a sleeping service woken first (on the
/// panel), and other servers that just failed skipped unless nothing else is left.
#[allow(clippy::result_large_err)]
async fn choose_upstream(app: &App, route: &Route, hop: bool, host: &str) -> Result<Upstream, Response<Body>> {
    let mut upstreams: Vec<Upstream> = route.upstreams.iter().filter(|u| !hop || matches!(u, Upstream::Local(_))).copied().collect();
    if upstreams.is_empty() && route.asleep && *crate::util::read(&app.proxy.me) == "local" {
        upstreams = crate::sleep::wake(app, route.service_id.as_deref().unwrap_or_default(), route.port).await.map_err(|e| {
            tracing::warn!("waking {host}: {e:#}");
            error_page(route, StatusCode::SERVICE_UNAVAILABLE, "Waking up failed", &e.to_string())
        })?;
    }
    {
        let now = std::time::Instant::now();
        let mut down = crate::util::lock(&app.proxy.down);
        down.retain(|_, until| *until > now);
        if upstreams.iter().any(|u| !matches!(u, Upstream::Node(a) if down.contains_key(a))) {
            upstreams.retain(|u| !matches!(u, Upstream::Node(a) if down.contains_key(a)));
        }
    }
    let pick = app.proxy.rr.fetch_add(1, Ordering::Relaxed) % upstreams.len().max(1);
    upstreams.get(pick).copied().ok_or_else(|| error_page(route, StatusCode::SERVICE_UNAVAILABLE, "Service unavailable", "The service is not running right now."))
}

/// The path and query a replica sees: the route's prefix stripped, then its rewrite applied.
fn upstream_path<'a>(route: &Route, path_and_query: &'a str) -> std::borrow::Cow<'a, str> {
    let mut pq = std::borrow::Cow::Borrowed(path_and_query);
    if route.options.strip_prefix
        && route.path != "/"
        && let Some(rest) = path_and_query.strip_prefix(route.path.trim_end_matches('/'))
    {
        pq = if rest.starts_with('/') { rest.into() } else { format!("/{rest}").into() };
    }
    if let Some((re, to)) = &route.rewrite {
        pq = re.replace(&pq, to.as_str()).into_owned().into();
    }
    pq
}

fn in_mesh(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[..2] == [10, 99])
}

/// Addresses that URLs from service settings must never reach: this server itself, the mesh
/// (other servers' agents, Docker and registry) and cloud metadata. RFC1918/CGNAT/ULA stay
/// reachable so a LAN or tailnet registry or auth service still works; only the link-local and
/// the known off-link metadata endpoints are refused (169.254.169.254 is the usual one, caught
/// by the link-local check; 100.100.100.200 and fd00:ec2::254 are the off-link exceptions).
pub fn internal(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    let metadata = matches!(ip, IpAddr::V4(v4) if v4.octets() == [100, 100, 100, 200])
        || matches!(ip, IpAddr::V6(v6) if v6.segments() == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254]);
    ip.is_loopback()
        || ip.is_unspecified()
        || in_mesh(ip)
        || metadata
        || matches!(ip, IpAddr::V4(v4) if v4.is_link_local())
        || matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80)
}

/// A client for `url` pinned to an address checked against `internal`, so the name cannot
/// resolve somewhere else by the time the request is made. None when the URL is not allowed.
pub async fn outside_client(url: &str, schemes: &[&str]) -> Option<reqwest::Client> {
    let u = reqwest::Url::parse(url).ok().filter(|u| schemes.contains(&u.scheme()))?;
    let host = u.host_str()?.trim_matches(['[', ']']).to_string();
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), u.port_or_known_default()?)).await.ok()?.collect();
    let first = *addrs.first()?;
    if addrs.iter().any(|a| internal(a.ip())) {
        return None;
    }
    crate::util::http(10).redirect(reqwest::redirect::Policy::none()).resolve(&host, first).build().ok()
}

/// The key authorization for an ACME challenge `token` (at `path`) for `host`.
async fn acme_answer(app: &App, host: &str, path: &str, token: &str) -> Option<String> {
    if let Some(key_auth) = crate::util::lock(&app.proxy.challenges).get(token) {
        return Some(key_auth.clone());
    }
    // The panel runs ACME; DNS may point at any server.
    if *crate::util::read(&app.proxy.me) == "local" {
        return None;
    }
    let r = crate::util::http(10).build().ok()?.get(format!("http://{}{path}", crate::mesh::PANEL_IP)).header("host", host).send().await.ok()?;
    r.error_for_status().ok()?.text().await.ok()
}

fn strip_hop_headers(h: &mut HeaderMap, keep_upgrade: bool) {
    for name in ["keep-alive", "proxy-connection", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding"] {
        h.remove(name);
    }
    if !keep_upgrade {
        h.remove(header::CONNECTION);
        h.remove(header::UPGRADE);
    }
}

async fn basic_auth_ok(app: &App, route: &Route, headers: &HeaderMap) -> bool {
    let Some(creds) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ")) else {
        return false;
    };
    let key = crate::util::sha256_hex(format!("{}|{creds}|{:?}", route.path, route.service_id));
    let now = crate::util::now();
    if crate::util::lock(&app.proxy.auth_cache).get(&key).is_some_and(|exp| *exp > now) {
        return true;
    }
    let Ok(decoded) = crate::crypto::unb64(creds) else { return false };
    let Ok(decoded) = String::from_utf8(decoded) else { return false };
    let Some((user, pass)) = decoded.split_once(':') else { return false };
    let Some(hash) = route.options.basic_auth.iter().find(|u| u.user == user).map(|u| u.hash.clone()) else { return false };
    let pass = pass.to_string();
    let ok = crate::auth::hashing(move || crate::auth::verify_password(&pass, &hash)).await.unwrap_or(false);
    if ok {
        crate::util::lock(&app.proxy.auth_cache).insert(key, now + 600);
    }
    ok
}

#[allow(clippy::result_large_err)]
async fn forward_auth(
    fa: &crate::model::ForwardAuth,
    req: &Request<Incoming>,
    scheme: &str,
    host: &str,
    client: IpAddr,
) -> Result<Vec<(HeaderName, HeaderValue)>, Response<Body>> {
    // Its answer can go back to the visitor, so it never reaches `internal` addresses.
    let Some(http) = outside_client(&fa.url, &["http", "https"]).await else {
        return Err(page(StatusCode::BAD_GATEWAY, "Sign-in service unavailable", "The authentication service's address is not allowed."));
    };
    let mut r = http
        .get(&fa.url)
        .header("x-forwarded-method", req.method().as_str())
        .header("x-forwarded-proto", scheme)
        .header("x-forwarded-host", host)
        .header("x-forwarded-uri", req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/"))
        .header("x-forwarded-for", client.to_string());
    for name in [header::AUTHORIZATION, header::COOKIE] {
        if let Some(v) = req.headers().get(&name).and_then(|v| v.to_str().ok()) {
            r = r.header(name.as_str(), v);
        }
    }
    match r.send().await {
        Ok(resp) if resp.status().is_success() => Ok(fa
            .response_headers
            .iter()
            .filter_map(|n| Some((HeaderName::from_bytes(n.as_bytes()).ok()?, resp.headers().get(n.as_str())?.clone())))
            .collect()),
        Ok(resp) => {
            let status = resp.status();
            let location = resp.headers().get(header::LOCATION).cloned();
            let cookies: Vec<HeaderValue> = resp.headers().get_all(header::SET_COOKIE).iter().cloned().collect();
            let mut out = html_page(status, crate::util::read_capped(resp, 1 << 20).await.unwrap_or_default());
            if let Some(l) = location {
                out.headers_mut().insert(header::LOCATION, l);
            }
            for c in cookies {
                out.headers_mut().append(header::SET_COOKIE, c);
            }
            Err(out)
        }
        Err(_) => Err(page(StatusCode::BAD_GATEWAY, "Sign-in service unavailable", "The authentication service did not answer.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_client_ips() {
        assert!(path_under("/api/x", "/api"));
        assert!(path_under("/api", "/api"));
        assert!(!path_under("/apix", "/api"));
        assert!(path_under("/anything", "/"));
        assert!(path_under("/api/x", "/api/") && !path_under("/api", "/api/"));
        let host = |h: &str| host_of(&Request::builder().header(header::HOST, h).body(()).unwrap());
        assert_eq!(host("Example.COM:8080"), "example.com");
        assert_eq!(host("example.com."), "example.com");
        assert_eq!(host("[::1]:8080"), "::1");
        assert_eq!(host("[::1]"), "::1");
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.2".parse().unwrap());
        let trusted: Vec<ipnet::IpNet> = vec!["10.0.0.0/8".parse().unwrap()];
        assert_eq!(client_ip(&h, "10.0.0.3".parse().unwrap(), &trusted), ("203.0.113.9".parse().unwrap(), true));
        // Untrusted peers cannot spoof their address.
        assert_eq!(client_ip(&h, "198.51.100.7".parse().unwrap(), &trusted), ("198.51.100.7".parse().unwrap(), false));
        // Mapped addresses in the chain count as IPv4.
        h.insert("x-forwarded-for", "::ffff:203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&h, "10.0.0.3".parse().unwrap(), &trusted).0, "203.0.113.9".parse::<IpAddr>().unwrap());
        assert!(in_mesh("10.99.0.2".parse().unwrap()));
        assert!(!in_mesh("10.98.0.2".parse().unwrap()));
        for ip in ["127.0.0.1", "::1", "0.0.0.0", "10.99.0.1", "169.254.169.254", "fe80::1", "::ffff:127.0.0.1"] {
            assert!(internal(ip.parse().unwrap()), "{ip}");
        }
        // Off-link cloud metadata is refused; LAN and tailnet (CGNAT) stay reachable.
        for ip in ["100.100.100.200", "fd00:ec2::254"] {
            assert!(internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["203.0.113.9", "192.168.1.10", "2001:db8::1", "100.64.0.1", "172.16.9.9"] {
            assert!(!internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn outside_client_refuses_internal_targets() {
        // IP-literal hosts resolve without the network, so this is deterministic.
        for bad in ["https://127.0.0.1/x", "https://10.99.0.1:5000/v2/", "http://169.254.169.254/latest/meta-data/", "https://[::1]/x", "https://[::ffff:10.99.0.1]/x"] {
            assert!(outside_client(bad, &["http", "https"]).await.is_none(), "{bad}");
        }
        // A public literal is allowed; a wrong scheme is not.
        assert!(outside_client("https://93.184.216.34/x", &["https"]).await.is_some());
        assert!(outside_client("http://93.184.216.34/x", &["https"]).await.is_none(), "scheme");
    }

    #[tokio::test]
    async fn pages_escape_what_they_show() {
        let body = page(StatusCode::NOT_FOUND, "Nothing here yet", "No service is set up for <script>.").into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("&lt;script&gt;"));
    }

    #[test]
    fn tables_compile_per_server() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        let spec = RouteSpec {
            host: "a.test".into(),
            path: "/".into(),
            service_id: Some("s".into()),
            project_id: None,
            https: false,
            via_tunnel: false,
            options: DomainOptions::default(),
            targets: vec![Target { node: "local".into(), addr: addr("172.20.0.2:80") }, Target { node: "n1".into(), addr: addr("172.21.0.2:80") }],
            asleep: false,
            port: 80,
        };
        let nodes = HashMap::from([("local".to_string(), addr("10.99.0.1:80")), ("n1".to_string(), addr("10.99.0.2:80"))]);
        let table = Table { routes: vec![spec], nodes, trusted_proxies: vec![], worker: false };
        let on_panel = compile(&table, "local");
        let r = &on_panel.hosts["a.test"][0];
        assert_eq!(r.upstreams, vec![Upstream::Local(addr("172.20.0.2:80")), Upstream::Node(addr("10.99.0.2:80"))]);
        let on_worker = compile(&table, "n1");
        let r = &on_worker.hosts["a.test"][0];
        assert_eq!(r.upstreams, vec![Upstream::Node(addr("10.99.0.1:80")), Upstream::Local(addr("172.21.0.2:80"))]);
        assert!(on_worker.trusted.iter().any(|n| n.contains(&"10.99.0.1".parse::<IpAddr>().unwrap())));
        // A sleeping service: workers hand its requests to the panel, which wakes it.
        let mut asleep = table.clone();
        asleep.routes[0].targets.clear();
        asleep.routes[0].asleep = true;
        assert_eq!(compile(&asleep, "n1").hosts["a.test"][0].upstreams, vec![Upstream::Node(addr("10.99.0.1:80"))]);
        assert!(compile(&asleep, "local").hosts["a.test"][0].upstreams.is_empty());
    }

    #[test]
    fn worker_table_drops_secrets_and_other_nodes() {
        use crate::model::BasicUser;
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        let opts = DomainOptions {
            basic_auth: vec![BasicUser { user: "a".into(), hash: "secret-hash".into(), password: None }],
            forward_auth: Some(crate::model::ForwardAuth { url: "https://auth.example".into(), response_headers: vec![] }),
            ..Default::default()
        };
        let mine = RouteSpec {
            host: "a.test".into(), path: "/".into(), service_id: Some("s".into()), project_id: Some("p".into()),
            https: true, via_tunnel: false, options: opts,
            targets: vec![Target { node: "n1".into(), addr: addr("172.21.0.2:80") }, Target { node: "local".into(), addr: addr("172.20.0.2:80") }],
            asleep: false, port: 80,
        };
        let other = RouteSpec {
            host: "b.test".into(), path: "/".into(), service_id: Some("s2".into()), project_id: Some("p2".into()),
            https: true, via_tunnel: false, options: DomainOptions::default(),
            targets: vec![Target { node: "local".into(), addr: addr("172.20.0.3:80") }],
            asleep: false, port: 80,
        };
        let table = Table { routes: vec![mine, other], nodes: HashMap::new(), trusted_proxies: vec!["1.2.3.0/24".parse().unwrap()], worker: false };
        let reduced = reduce_for_worker(&table, "n1");
        assert_eq!(reduced.routes.len(), 1, "only routes with a target on n1");
        let r = &reduced.routes[0];
        assert_eq!(r.host, "a.test");
        assert!(r.options.basic_auth.is_empty(), "auth hashes stripped");
        assert!(r.options.forward_auth.is_none(), "forward auth stripped");
        assert_eq!(r.targets.len(), 1, "only n1's own target");
        assert_eq!(r.targets[0].node, "n1");
        assert!(reduced.nodes.is_empty() && reduced.trusted_proxies.is_empty());
        assert!(reduced.worker);
        assert!(compile(&reduced, "n1").worker);
    }

    #[test]
    fn replicas_see_stripped_and_rewritten_paths() {
        let route = |path: &str, strip: bool, rewrite: Option<(&str, &str)>| Route {
            port: 80,
            service_id: None,
            path: path.into(),
            upstreams: vec![],
            https: false,
            via_tunnel: false,
            options: DomainOptions { strip_prefix: strip, ..Default::default() },
            redirects: vec![],
            rewrite: rewrite.map(|(from, to)| (regex::Regex::new(from).unwrap(), to.to_string())),
            asleep: false,
        };
        assert_eq!(upstream_path(&route("/api", true, None), "/api/x?q=1"), "/x?q=1");
        assert_eq!(upstream_path(&route("/api/", true, None), "/api"), "/");
        assert_eq!(upstream_path(&route("/api", false, None), "/api/x"), "/api/x");
        assert_eq!(upstream_path(&route("/", true, None), "/x"), "/x");
        assert_eq!(upstream_path(&route("/api", true, Some(("^/v1/(.*)", "/v2/$1"))), "/api/v1/a"), "/v2/a");
    }

}
