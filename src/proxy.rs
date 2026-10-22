//! The built-in reverse proxy: host/path routing to live replicas, HTTPS with automatic
//! certificates, WebSockets, gRPC, redirects, auth and maintenance pages.

use crate::app::{App, AppState};
use crate::model::{DomainOptions, WwwRedirect};
use crate::{runtime, store};
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

pub struct Route {
    pub service_id: Option<String>,
    pub project_id: Option<String>,
    pub path: String,
    pub upstreams: Vec<SocketAddr>,
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
}

impl Routes {
    pub fn find(&self, host: &str, path: &str) -> Option<Arc<Route>> {
        self.hosts.get(host)?.iter().find(|r| path_under(path, &r.path)).cloned()
    }
}

fn path_under(path: &str, prefix: &str) -> bool {
    prefix == "/" || path == prefix || path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
}

#[derive(Default, Clone, Copy)]
pub struct Traffic {
    pub requests: u64,
    pub last_at: i64,
}

pub struct Proxy {
    routes: RwLock<Arc<Routes>>,
    certs: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    fallback: Arc<CertifiedKey>,
    pub challenges: Mutex<HashMap<String, String>>,
    rr: AtomicUsize,
    pub traffic: Mutex<HashMap<String, Traffic>>,
    reload: tokio::sync::Notify,
    http1: Client<HttpConnector, Body>,
    h2c: Client<HttpConnector, Body>,
    auth_cache: Mutex<HashMap<String, i64>>,
}

impl Proxy {
    pub fn new() -> Self {
        let mut conn = HttpConnector::new();
        conn.set_connect_timeout(Some(Duration::from_secs(5)));
        conn.set_nodelay(true);
        let self_signed = rcgen::generate_simple_self_signed(vec!["tugport.invalid".into()]).expect("self-signed certificate");
        let fallback = certified_key(&self_signed.cert.pem(), &self_signed.signing_key.serialize_pem()).expect("fallback key");
        Proxy {
            routes: RwLock::new(Arc::new(Routes::default())),
            certs: RwLock::new(HashMap::new()),
            fallback: Arc::new(fallback),
            challenges: Mutex::new(HashMap::new()),
            rr: AtomicUsize::new(0),
            traffic: Mutex::new(HashMap::new()),
            reload: tokio::sync::Notify::new(),
            http1: Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(conn.clone()),
            h2c: Client::builder(TokioExecutor::new()).http2_only(true).build(conn),
            auth_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn routes(&self) -> Arc<Routes> {
        self.routes.read().unwrap().clone()
    }

    pub fn set_cert(&self, host: &str, key: CertifiedKey) {
        self.certs.write().unwrap().insert(host.to_string(), Arc::new(key));
    }

    pub fn has_cert(&self, host: &str) -> bool {
        self.certs.read().unwrap().contains_key(host)
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
        let certs = p.certs.read().unwrap();
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

async fn build_routes(app: &App) -> Result<Routes> {
    let mut hosts: HashMap<String, Vec<Arc<Route>>> = HashMap::new();
    let services: HashMap<String, store::Service> = store::services(app, None)?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut replicas: HashMap<String, Vec<runtime::Replica>> = HashMap::new();
    for d in store::domains(app, None)? {
        let Some(s) = services.get(&d.service_id) else { continue };
        if !replicas.contains_key(&s.id) {
            replicas.insert(s.id.clone(), runtime::replicas(app, &s.id).await.unwrap_or_default());
        }
        let live = runtime::live_deploy(app, &s.id);
        let upstreams = replicas[&s.id]
            .iter()
            .filter(|r| r.state == "running")
            .filter(|r| match (&d.container, s.kind.as_str()) {
                (Some(c), "compose") => r.compose_service.as_deref() == Some(c.as_str()),
                (None, "compose") => true,
                _ => live.as_deref().is_none_or(|l| r.deploy == l),
            })
            .filter_map(|r| r.ip.as_ref()?.parse::<IpAddr>().ok())
            .map(|ip| SocketAddr::new(ip, d.port))
            .collect();
        let redirects = d
            .options
            .redirects
            .iter()
            .filter_map(|r| regex::Regex::new(&r.from).ok().map(|re| (re, r.to.clone(), r.permanent)))
            .collect();
        let rewrite = d.options.rewrite.as_ref().and_then(|r| regex::Regex::new(&r.from).ok().map(|re| (re, r.to.clone())));
        let route = Route {
            service_id: Some(s.id.clone()),
            project_id: Some(s.project_id.clone()),
            path: d.path.clone(),
            upstreams,
            https: d.https,
            via_tunnel: d.via_tunnel,
            options: d.options.clone(),
            redirects,
            rewrite,
            asleep: crate::sleep::is_asleep(app, &s.id),
        };
        hosts.entry(d.host.clone()).or_default().push(Arc::new(route));
    }
    let settings = app.settings();
    if let Some(panel) = &settings.panel_domain {
        let port = settings.panel_listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(8484);
        hosts.entry(panel.clone()).or_default().push(Arc::new(Route {
            service_id: None,
            project_id: None,
            path: "/".into(),
            upstreams: vec![SocketAddr::from(([127, 0, 0, 1], port))],
            https: true,
            via_tunnel: false,
            options: DomainOptions { timeout_s: Some(3600), ..Default::default() },
            redirects: vec![],
            rewrite: None,
            asleep: false,
        }));
    }
    for routes in hosts.values_mut() {
        routes.sort_by_key(|r| std::cmp::Reverse(r.path.len()));
    }
    Ok(Routes { hosts })
}

/// Keep the routing table fresh: on demand, on container events, and every 30 seconds.
pub async fn watch(app: AppState) {
    {
        let app = app.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let filters = HashMap::from([("type".to_string(), vec!["container".to_string()])]);
            loop {
                let mut events = app.docker.events(Some(bollard::query_parameters::EventsOptions { filters: Some(filters.clone()), ..Default::default() }));
                while let Some(Ok(e)) = events.next().await {
                    if matches!(e.action.as_deref(), Some("start" | "die" | "stop" | "pause" | "unpause")) || e.action.as_deref().is_some_and(|a| a.starts_with("health_status")) {
                        reload(&app);
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }
    loop {
        match build_routes(&app).await {
            Ok(r) => *app.proxy.routes.write().unwrap() = Arc::new(r),
            Err(e) => tracing::error!("building routes: {e:#}"),
        }
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
    let (http, https) = tokio::try_join!(bind(http_port), bind(https_port))?;
    tracing::info!("proxy listening on :{http_port} and :{https_port}");
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = http.accept().await else { continue };
            let app = a.clone();
            tokio::spawn(async move { serve_conn(app, TokioIo::new(stream), peer, false).await });
        }
    });
    loop {
        let Ok((stream, peer)) = https.accept().await else { continue };
        let (app, tls) = (app.clone(), tls.clone());
        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await {
                Ok(Ok(s)) => serve_conn(app, TokioIo::new(s), peer, true).await,
                _ => {}
            }
        });
    }
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
    let svc = hyper::service::service_fn(move |req| {
        let app = app.clone();
        async move { Ok::<_, std::convert::Infallible>(handle(app, req, peer, tls).await) }
    });
    let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(io, svc).await;
}

fn full(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(body.into()).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn redirect(status: StatusCode, to: &str) -> Response<Body> {
    let mut r = full(status, "text/plain", format!("Redirecting to {to}\n"));
    r.headers_mut().insert(header::LOCATION, HeaderValue::from_str(to).unwrap_or(HeaderValue::from_static("/")));
    r
}

pub fn page(status: StatusCode, title: &str, text: &str) -> Response<Body> {
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
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let client = chain.iter().rev().find(|ip| !is_trusted(ip)).copied().or(chain.first().copied()).unwrap_or(peer);
    (client, true)
}

async fn handle(app: AppState, mut req: Request<Incoming>, peer: SocketAddr, tls: bool) -> Response<Body> {
    let host = host_of(&req);
    let path = req.uri().path().to_string();

    if !tls {
        if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
            return match app.proxy.challenges.lock().unwrap().get(token) {
                Some(key_auth) => full(StatusCode::OK, "text/plain", key_auth.clone()),
                None => full(StatusCode::NOT_FOUND, "text/plain", "unknown challenge"),
            };
        }
    }

    let routes = app.proxy.routes();
    let Some(route) = routes.find(&host, &path) else {
        return page(StatusCode::NOT_FOUND, "Nothing here yet", &format!("No service is set up for {host}."));
    };
    let settings_trusted = app.settings().trusted_proxies;
    let (client, via_trusted) = client_ip(req.headers(), peer.ip(), &settings_trusted);
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
    if !route.options.basic_auth.is_empty() && !basic_auth_ok(&app, &route, req.headers()) {
        let mut r = page(StatusCode::UNAUTHORIZED, "Sign in", "This site needs a user name and password.");
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Restricted\", charset=\"UTF-8\""));
        return r;
    }
    let mut extra_headers = vec![];
    if let Some(fa) = &route.options.forward_auth {
        match forward_auth(&app, fa, &req, scheme, &host, client).await {
            Ok(h) => extra_headers = h,
            Err(resp) => return resp,
        }
    }

    if let Some(sid) = &route.service_id {
        let mut t = app.proxy.traffic.lock().unwrap();
        let e = t.entry(sid.clone()).or_default();
        e.requests += 1;
        e.last_at = crate::util::now();
    }
    let upstreams = if route.upstreams.is_empty() && route.asleep {
        match crate::sleep::wake(&app, route.service_id.as_deref().unwrap_or_default()).await {
            Ok(u) => u.into_iter().map(|ip| SocketAddr::new(ip, route.upstreams.first().map(|u| u.port()).unwrap_or(80))).collect(),
            Err(e) => return error_page(Some(&route), StatusCode::SERVICE_UNAVAILABLE, "Waking up failed", &e.to_string()),
        }
    } else {
        route.upstreams.clone()
    };
    if upstreams.is_empty() {
        return error_page(Some(&route), StatusCode::SERVICE_UNAVAILABLE, "Service unavailable", "The service is not running right now.");
    }
    let upstream = upstreams[app.proxy.rr.fetch_add(1, Ordering::Relaxed) % upstreams.len()];

    // Path handling: strip the route prefix, then apply a rewrite.
    let mut new_pq = path_and_query.clone();
    if route.options.strip_prefix && route.path != "/" {
        let prefix = route.path.trim_end_matches('/');
        new_pq = new_pq.strip_prefix(prefix).map(|r| if r.starts_with('/') { r.to_string() } else { format!("/{r}") }).unwrap_or(new_pq);
    }
    if let Some((re, to)) = &route.rewrite {
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
            return error_page(Some(&route), StatusCode::BAD_GATEWAY, "Bad gateway", "The service did not answer. It may be starting or crashing.");
        }
        Err(_) => return error_page(Some(&route), StatusCode::GATEWAY_TIMEOUT, "Gateway timeout", "The service took too long to answer."),
    };

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = client_upgrade {
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
    }
    let (mut rparts, rbody) = resp.into_parts();
    if (502..=504).contains(&rparts.status.as_u16()) {
        if let Some(custom) = route.options.error_pages.get(&rparts.status.as_u16()) {
            return full(rparts.status, "text/html; charset=utf-8", custom.clone());
        }
    }
    strip_hop_headers(&mut rparts.headers, false);
    Response::from_parts(rparts, rbody.map_err(|e| Box::new(e) as BoxErr).boxed())
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

fn basic_auth_ok(app: &App, route: &Route, headers: &HeaderMap) -> bool {
    let Some(creds) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ")) else {
        return false;
    };
    let key = crate::util::sha256_hex(format!("{}|{creds}|{:?}", route.path, route.service_id));
    let now = crate::util::now();
    if app.proxy.auth_cache.lock().unwrap().get(&key).is_some_and(|exp| *exp > now) {
        return true;
    }
    let Ok(decoded) = crate::crypto::unb64(creds) else { return false };
    let Ok(decoded) = String::from_utf8(decoded) else { return false };
    let Some((user, pass)) = decoded.split_once(':') else { return false };
    let ok = route.options.basic_auth.iter().any(|u| u.user == user && crate::auth::verify_password(pass, &u.hash));
    if ok {
        app.proxy.auth_cache.lock().unwrap().insert(key, now + 600);
    }
    ok
}

async fn forward_auth(
    app: &App,
    fa: &crate::model::ForwardAuth,
    req: &Request<Incoming>,
    scheme: &str,
    host: &str,
    client: IpAddr,
) -> Result<Vec<(HeaderName, HeaderValue)>, Response<Body>> {
    let _ = app;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).build().unwrap();
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
            let body = resp.bytes().await.unwrap_or_default();
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
    }
}
