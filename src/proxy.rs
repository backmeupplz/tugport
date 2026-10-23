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
use std::sync::atomic::{AtomicUsize, Ordering};
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

pub struct Route {
    pub port: u16,
    pub service_id: Option<String>,
    pub project_id: Option<String>,
    pub path: String,
    pub upstreams: Vec<Upstream>,
    pub https: bool,
    pub via_tunnel: bool,
    pub options: DomainOptions,
    redirects: Vec<(regex::Regex, String, bool)>,
    rewrite: Option<(regex::Regex, String)>,
    /// Service is scaled to zero; the first request wakes it.
    pub asleep: bool,
}

#[derive(Default)]
pub struct Routes {
    pub hosts: HashMap<String, Vec<Arc<Route>>>,
    pub trusted: Vec<ipnet::IpNet>,
}

/// Turn a table into routes as seen from node `me`.
pub fn compile(table: &Table, me: &str) -> Routes {
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
            project_id: spec.project_id.clone(),
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
    if let Ok(mesh) = "10.99.0.0/16".parse() {
        trusted.push(mesh);
    }
    Routes { hosts, trusted }
}

impl Routes {
    pub fn find(&self, host: &str, path: &str) -> Option<Arc<Route>> {
        self.hosts.get(host)?.iter().find(|r| path_under(path, &r.path)).cloned()
    }
}

fn path_under(path: &str, prefix: &str) -> bool {
    prefix == "/" || path == prefix || path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
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
            down: Mutex::new(HashMap::new()),
            draining: Mutex::new(Default::default()),
            remote_traffic: Mutex::new(HashMap::new()),
            traffic: Mutex::new(HashMap::new()),
            reload: tokio::sync::Notify::new(),
            http1: Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(conn.clone()),
            h2c: Client::builder(TokioExecutor::new()).http2_only(true).build(conn),
            auth_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn routes(&self) -> Arc<Routes> {
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
    let mut replicas = runtime::all_replicas(app).await.unwrap_or_default();
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
            .entry(s.id.clone())
            .or_default()
            .iter()
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
    Ok(Table { routes, nodes: crate::nodes::proxy_addresses(app), trusted_proxies: settings.trusted_proxies })
}

async fn rebuild(app: &App) {
    match build_table(app).await {
        Ok(t) => {
            let changed = *app.proxy.table() != t;
            app.proxy.set_table(t);
            if changed {
                crate::nodes::push_routes(app);
            }
        }
        Err(e) => tracing::error!("building routes: {e:#}"),
    }
}

/// Rebuild now and wait until every server has the new table (a woken service's
/// requests may be forwarded to another server right away).
pub async fn rebuild_now(app: &App) {
    match build_table(app).await {
        Ok(t) => {
            app.proxy.set_table(t);
            crate::nodes::push_routes_now(app).await;
        }
        Err(e) => tracing::error!("building routes: {e:#}"),
    }
}

/// Build the routing table once; call before serving so the first request already routes.
pub async fn initial_routes(app: &App) {
    rebuild(app).await;
}

/// Keep the routing table fresh: on demand, on container events on any server, and every 30 seconds.
pub async fn watch(app: AppState) {
    tokio::spawn(crate::nodes::for_each_node(app.clone(), |app, node| async move {
        use futures_util::StreamExt;
        let filters = HashMap::from([("type".to_string(), vec!["container".to_string()])]);
        let mut events = node.docker.events(Some(bollard::query_parameters::EventsOptions { filters: Some(filters), ..Default::default() }));
        while let Some(Ok(e)) = events.next().await {
            if matches!(e.action.as_deref(), Some("start" | "die" | "stop" | "pause" | "unpause")) || e.action.as_deref().is_some_and(|a| a.starts_with("health_status")) {
                reload(&app);
            }
        }
    }));
    loop {
        rebuild(&app).await;
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
    let a = app.clone();
    tokio::spawn(async move {
        while !crate::handoff::draining() {
            let accepted = tokio::select! {
                r = http.accept() => r,
                _ = crate::handoff::DRAIN.notified() => break,
            };
            let Ok((stream, peer)) = accepted else { continue };
            let app = a.clone();
            tokio::spawn(async move { serve_conn(app, TokioIo::new(stream), peer, false).await });
        }
    });
    while !crate::handoff::draining() {
        let accepted = tokio::select! {
            r = https.accept() => r,
            _ = crate::handoff::DRAIN.notified() => break,
        };
        let Ok((stream, peer)) = accepted else { continue };
        let (app, tls) = (app.clone(), tls.clone());
        tokio::spawn(async move {
            if let Ok(Ok(s)) = tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await {
                serve_conn(app, TokioIo::new(s), peer, true).await
            }
        });
    }
    Ok(())
}

/// Bind, waiting while another proxy (Traefik during a migration) still holds the port.
async fn bind(port: u16) -> Result<tokio::net::TcpListener> {
    let mut warned = false;
    loop {
        match tokio::net::TcpListener::bind(("::", port)).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(_) => match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => return Ok(l),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
                Err(e) => return Err(e.into()),
            },
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

fn full(status: StatusCode, content_type: &'static str, body: impl Into<Bytes>) -> Response<Body> {
    let mut r = Response::new(Full::new(body.into()).map_err(|e| match e {}).boxed());
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
    full(status, "text/html; charset=utf-8", html)
}

fn error_page(route: Option<&Route>, status: StatusCode, title: &str, text: &str) -> Response<Body> {
    match route.and_then(|r| r.options.error_pages.get(&status.as_u16())) {
        Some(custom) => full(status, "text/html; charset=utf-8", custom.clone()),
        None => page(status, title, text),
    }
}

fn host_of(req: &Request<Incoming>) -> String {
    let raw = req.uri().authority().map(|a| a.as_str().to_string()).or_else(|| req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(String::from)).unwrap_or_default();
    let host = match raw.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|c| c.is_ascii_digit()) && !h.ends_with(']') || raw.starts_with('[') => h.trim_matches(['[', ']']).to_string(),
        _ => raw,
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
    let path = req.uri().path().to_string();

    if !tls
        && let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
            let known = crate::util::lock(&app.proxy.challenges).get(token).cloned();
            return match known {
                Some(key_auth) => full(StatusCode::OK, "text/plain", key_auth),
                // The panel runs ACME; DNS may point at any server.
                None if *crate::util::read(&app.proxy.me) != "local" => panel_challenge(&host, &path).await,
                None => full(StatusCode::NOT_FOUND, "text/plain", "unknown challenge"),
            };
        }

    let routes = app.proxy.routes();
    let Some(route) = routes.find(&host, &path) else {
        return page(StatusCode::NOT_FOUND, "Nothing here yet", &format!("No service is set up for {host}."));
    };
    let (client, via_trusted) = client_ip(req.headers(), peer.ip(), &routes.trusted);
    // A neighbour proxy already chose this server and checked auth: serve from local
    // replicas only. Only Tugport itself can open connections from the mesh.
    let hop = req.headers().contains_key(HOP) && in_mesh(peer.ip());
    let forwarded_https = via_trusted && req.headers().get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let scheme = if tls || forwarded_https { "https" } else { "http" };
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());

    if route.https && scheme == "http" && !route.via_tunnel && app.proxy.has_cert(&host) {
        return redirect(StatusCode::PERMANENT_REDIRECT, &format!("https://{host}{path_and_query}"));
    }
    if let Some(html) = &route.options.maintenance {
        return full(StatusCode::SERVICE_UNAVAILABLE, "text/html; charset=utf-8", html.clone());
    }
    match route.options.www_redirect {
        Some(WwwRedirect::ToWww) if !host.starts_with("www.") => {
            return redirect(StatusCode::MOVED_PERMANENTLY, &format!("{scheme}://www.{host}{path_and_query}"));
        }
        Some(WwwRedirect::ToApex) if host.starts_with("www.") => {
            return redirect(StatusCode::MOVED_PERMANENTLY, &format!("{scheme}://{}{path_and_query}", &host[4..]));
        }
        _ => {}
    }
    let full_url = format!("{scheme}://{host}{path_and_query}");
    for (re, to, permanent) in &route.redirects {
        if re.is_match(&full_url) {
            let target = re.replace(&full_url, to.as_str()).to_string();
            return redirect(if *permanent { StatusCode::PERMANENT_REDIRECT } else { StatusCode::FOUND }, &target);
        }
    }
    if !hop && !route.options.basic_auth.is_empty() && !basic_auth_ok(&app, &route, req.headers()).await {
        let mut r = page(StatusCode::UNAUTHORIZED, "Sign in", "This site needs a user name and password.");
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Restricted\", charset=\"UTF-8\""));
        return r;
    }
    let mut extra_headers = vec![];
    if let Some(fa) = route.options.forward_auth.as_ref().filter(|_| !hop) {
        match forward_auth(&app, fa, &req, scheme, &host, client).await {
            Ok(h) => extra_headers = h,
            Err(resp) => return resp,
        }
    }

    // Counted once, by the first proxy (scale-to-zero and autoscaling read these).
    if !hop
        && let Some(sid) = &route.service_id
    {
        let mut t = crate::util::lock(&app.proxy.traffic);
        let e = t.entry(sid.clone()).or_default();
        e.requests += 1;
        e.last_at = crate::util::now();
    }
    let mut upstreams: Vec<Upstream> = route.upstreams.iter().filter(|u| !hop || matches!(u, Upstream::Local(_))).copied().collect();
    if upstreams.is_empty() && route.asleep && *crate::util::read(&app.proxy.me) == "local" {
        match crate::sleep::wake(&app, route.service_id.as_deref().unwrap_or_default(), route.port).await {
            Ok(woken) => upstreams = woken,
            Err(e) => {
                tracing::warn!("waking {host}: {e:#}");
                return error_page(Some(&route), StatusCode::SERVICE_UNAVAILABLE, "Waking up failed", &e.to_string());
            }
        }
    }
    if upstreams.is_empty() {
        return error_page(Some(&route), StatusCode::SERVICE_UNAVAILABLE, "Service unavailable", "The service is not running right now.");
    }
    // Skip servers that just failed, unless nothing else is left.
    {
        let now = std::time::Instant::now();
        let mut down = crate::util::lock(&app.proxy.down);
        down.retain(|_, until| *until > now);
        let up: Vec<Upstream> = upstreams.iter().filter(|u| !matches!(u, Upstream::Node(a) if down.contains_key(a))).copied().collect();
        if !up.is_empty() {
            upstreams = up;
        }
    }
    let upstream = upstreams[app.proxy.rr.fetch_add(1, Ordering::Relaxed) % upstreams.len()];
    let (upstream, via_node) = match upstream {
        Upstream::Local(a) => (a, false),
        Upstream::Node(a) => (a, true),
    };

    // Path handling: strip the route prefix, then apply a rewrite.
    let mut new_pq = path_and_query.clone();
    if route.options.strip_prefix && route.path != "/" && !via_node {
        let prefix = route.path.trim_end_matches('/');
        new_pq = new_pq.strip_prefix(prefix).map(|r| if r.starts_with('/') { r.to_string() } else { format!("/{r}") }).unwrap_or(new_pq);
    }
    if let Some((re, to)) = route.rewrite.as_ref().filter(|_| !via_node) {
        new_pq = re.replace(&new_pq, to.as_str()).to_string();
    }

    let grpc = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("application/grpc"));
    let upgrade = req.headers().get(header::UPGRADE).cloned();
    let client_upgrade = upgrade.is_some().then(|| hyper::upgrade::on(&mut req));

    let (mut parts, body) = req.into_parts();
    parts.uri = match Uri::builder().scheme("http").authority(upstream.to_string()).path_and_query(new_pq.as_str()).build() {
        Ok(u) => u,
        Err(_) => return page(StatusCode::BAD_REQUEST, "Bad request", "The address is not valid."),
    };
    strip_hop_headers(&mut parts.headers, upgrade.is_some());
    if let Ok(h) = HeaderValue::from_str(&host) {
        parts.headers.insert(header::HOST, h);
    }
    let xff = match parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).filter(|_| via_trusted) {
        Some(prev) => format!("{prev}, {}", peer.ip()),
        None => client.to_string(),
    };
    for (k, v) in [
        ("x-forwarded-for", xff),
        ("x-real-ip", client.to_string()),
        ("x-forwarded-proto", scheme.to_string()),
        ("x-forwarded-host", host.clone()),
    ] {
        if let Ok(v) = HeaderValue::from_str(&v) {
            parts.headers.insert(HeaderName::from_static(k), v);
        }
    }
    for (k, v) in extra_headers {
        parts.headers.insert(k, v);
    }
    if via_node {
        parts.headers.insert(HeaderName::from_static(HOP), HeaderValue::from_static("1"));
    }
    let limit = route.options.max_body_mb.map(|mb| (mb * 1024 * 1024) as usize);
    let body: Body = match limit {
        Some(l) => Limited::new(body, l).boxed(),
        None => body.map_err(|e| Box::new(e) as BoxErr).boxed(),
    };
    let client = if grpc && parts.version == Version::HTTP_2 {
        &app.proxy.h2c
    } else {
        parts.version = Version::HTTP_11;
        &app.proxy.http1
    };
    let timeout = Duration::from_secs(route.options.timeout_s.unwrap_or(300));
    let resp = match tokio::time::timeout(timeout, client.request(Request::from_parts(parts, body))).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!("upstream {upstream} for {host}: {e}");
            if via_node && e.is_connect() {
                crate::util::lock(&app.proxy.down).insert(upstream, std::time::Instant::now() + Duration::from_secs(15));
                reload(&app);
            }
            return error_page(Some(&route), StatusCode::BAD_GATEWAY, "Bad gateway", "The service did not answer. It may be starting or crashing.");
        }
        Err(_) => return error_page(Some(&route), StatusCode::GATEWAY_TIMEOUT, "Gateway timeout", "The service took too long to answer."),
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_upgrade) = client_upgrade {
            let (mut rparts, rbody) = resp.into_parts();
            let mut upstream_resp = Response::from_parts(rparts.clone(), rbody);
            let upstream_upgrade = hyper::upgrade::on(&mut upstream_resp);
            tokio::spawn(async move {
                if let (Ok(c), Ok(u)) = (client_upgrade.await, upstream_upgrade.await) {
                    let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(c), &mut TokioIo::new(u)).await;
                }
            });
            rparts.headers.remove(header::CONTENT_LENGTH);
            return Response::from_parts(rparts, Full::new(Bytes::new()).map_err(|e| match e {}).boxed());
        }
    let (mut rparts, rbody) = resp.into_parts();
    if (502..=504).contains(&rparts.status.as_u16())
        && let Some(custom) = route.options.error_pages.get(&rparts.status.as_u16()) {
            return full(rparts.status, "text/html; charset=utf-8", custom.clone());
        }
    strip_hop_headers(&mut rparts.headers, false);
    Response::from_parts(rparts, rbody.map_err(|e| Box::new(e) as BoxErr).boxed())
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
    reqwest::Client::builder().timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).resolve(&host, first).build().ok()
}

/// Forward auth's answer can go back to the visitor, so it never reaches `internal` addresses.
async fn forward_auth_client(url: &str) -> Option<reqwest::Client> {
    outside_client(url, &["http", "https"]).await
}

async fn panel_challenge(host: &str, path: &str) -> Response<Body> {
    let url = format!("http://{}{path}", crate::mesh::PANEL_IP);
    let answer = async {
        let r = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok()?.get(url).header("host", host).send().await.ok()?;
        if r.status().is_success() { r.text().await.ok() } else { None }
    };
    match answer.await {
        Some(key_auth) => full(StatusCode::OK, "text/plain", key_auth),
        None => full(StatusCode::NOT_FOUND, "text/plain", "unknown challenge"),
    }
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
    let Some((user, pass)) = decoded.split_once(':').map(|(u, p)| (u.to_string(), p.to_string())) else { return false };
    let Some(hash) = route.options.basic_auth.iter().find(|u| u.user == user).map(|u| u.hash.clone()) else { return false };
    let ok = crate::auth::hashing(move || crate::auth::verify_password(&pass, &hash)).await.unwrap_or(false);
    if ok {
        crate::util::lock(&app.proxy.auth_cache).insert(key, now + 600);
    }
    ok
}

#[allow(clippy::result_large_err)]
async fn forward_auth(
    app: &App,
    fa: &crate::model::ForwardAuth,
    req: &Request<Incoming>,
    scheme: &str,
    host: &str,
    client: IpAddr,
) -> Result<Vec<(HeaderName, HeaderValue)>, Response<Body>> {
    let _ = app;
    let Some(http) = forward_auth_client(&fa.url).await else {
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
            .filter_map(|n| {
                let v = resp.headers().get(n.as_str())?.to_str().ok()?;
                Some((HeaderName::from_bytes(n.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?))
            })
            .collect()),
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::UNAUTHORIZED);
            let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(String::from);
            let set_cookie: Vec<String> = resp.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok().map(String::from)).collect();
            let body = crate::util::read_capped(resp, 1 << 20).await.unwrap_or_default();
            let mut out = full(status, "text/html; charset=utf-8", body);
            if let Some(l) = location.and_then(|l| HeaderValue::from_str(&l).ok()) {
                out.headers_mut().insert(header::LOCATION, l);
            }
            for c in set_cookie.into_iter().filter_map(|c| HeaderValue::from_str(&c).ok()) {
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
        let table = Table { routes: vec![spec], nodes, trusted_proxies: vec![] };
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
}
