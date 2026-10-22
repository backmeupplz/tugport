mod acme;
mod alerts;
mod api;
mod app;
mod auth;
mod autoupdate;
mod backup;
mod builder;
mod cleanup;
mod compose;
mod crypto;
mod databases;
mod db;
mod deploy;
mod git;
mod logs;
mod metrics;
mod model;
mod notify;
mod paths;
mod proc;
mod proxy;
mod registry;
mod runtime;
mod secrets;
mod shim;
mod sleep;
mod store;
mod tools;
mod tunnel;
mod util;
mod web;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = match option_env!("TUGPORT_COMMIT") {
    Some(c) => c,
    None => "dev",
};

#[derive(Parser)]
#[command(name = "tugport", version, about = "Deploy your apps and watch your server from one binary")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the panel, proxy and deploy engine.
    Server,
    /// Print a one-time link for creating the owner account.
    SetupLink,
    /// Restore the master key from its recovery key after a reinstall.
    Recover { key: String },
    /// Print the version.
    Version,
}

fn main() -> Result<()> {
    // Containers start through this binary mounted as /run/tugport/init.
    if shim::invoked_as_init() {
        shim::run(std::env::args().skip(1).collect());
    }
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Version => {
            println!("tugport {VERSION} ({COMMIT})");
            Ok(())
        }
        Cmd::Recover { key } => {
            let paths = paths::Paths::from_env();
            crypto::restore_master(&paths.etc, &key)?;
            println!("Master key restored to {}", paths.etc.display());
            Ok(())
        }
        Cmd::SetupLink => {
            let paths = paths::Paths::from_env();
            let db = db::Db::open(&paths.db())?;
            match setup_link(&db)? {
                Some(link) => println!("Open {link} to create the owner account."),
                None => println!("Setup is already done."),
            }
            Ok(())
        }
        Cmd::Server => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve()),
    }
}

/// Fresh one-time setup link, or None once an owner exists.
fn setup_link(db: &db::Db) -> Result<Option<String>> {
    let users: i64 = db.conn().query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if users > 0 {
        return Ok(None);
    }
    let token = util::token(32);
    db.set_state("setup_token", &util::sha256_hex(&token))?;
    let host = public_ip().unwrap_or_else(|| "localhost".into());
    Ok(Some(format!("http://{host}:8484/setup#{token}")))
}

fn public_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

/// Containers mount a static copy of this binary as their init. `TUGPORT_INIT` overrides
/// the source (development builds link glibc and cannot run in every image).
fn install_init_binary(app: &app::App) -> Result<()> {
    let src = match std::env::var_os("TUGPORT_INIT") {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_exe()?,
    };
    let dest = app.paths.init_binary();
    let same = std::fs::read(&dest).ok().is_some_and(|d| std::fs::read(&src).ok().is_some_and(|s| s == d));
    if !same {
        let tmp = dest.with_extension("new");
        std::fs::copy(&src, &tmp).with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
        paths::restrict(&tmp, 0o755)?;
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(())
}

async fn serve() -> Result<()> {
    use tracing_subscriber::prelude::*;
    let paths = paths::Paths::from_env();
    paths.ensure().with_context(|| format!("creating {} and {}", paths.data.display(), paths.etc.display()))?;
    let selflog = logs::SelfLog::new(paths.logs().join("_tugport"));
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "tugport=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::fmt::layer().json().with_ansi(false).with_writer(logs::SelfLogHandle(selflog.clone())))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let master = match crypto::load_master(&paths.etc) {
        Ok(k) => k,
        Err(_) if !paths.etc.join("master.key").exists() && !paths.etc.join("master.key.cred").exists() => {
            let (k, store) = crypto::create_master(&paths.etc)?;
            eprintln!(
                "Created a new master key ({store:?}). Save this recovery key somewhere safe; it is shown only once:\n\n  {}\n",
                crypto::recovery_key(&k)
            );
            k
        }
        Err(e) => return Err(e),
    };
    let db = db::Db::open(&paths.db())?;
    let docker = bollard::Docker::connect_with_local_defaults().context("connecting to Docker")?;
    if let Some(link) = setup_link(&db)? {
        tracing::info!("open {link} to create the owner account");
    }

    let (events, _) = tokio::sync::broadcast::channel(4096);
    let slots = db.get_setting::<app::Settings>("instance")?.unwrap_or_default().builds.max_parallel.max(1) as usize;
    let app: app::AppState = Arc::new(app::App {
        paths,
        db,
        keys: crypto::Keyring::new(&master),
        docker,
        events,
        deployer: deploy::Deployer::default(),
        build_slots: Arc::new(tokio::sync::Semaphore::new(slots)),
        proxy: proxy::Proxy::new()?,
        logs: tokio::sync::broadcast::channel(8192).0,
        selflog,
        metrics: metrics::Metrics::default(),
    });
    drop(master);
    install_init_binary(&app)?;
    deploy::recover_interrupted(&app)?;
    match secrets::rematerialize(&app).await {
        Ok(n) if n > 0 => tracing::info!("restored secrets for {n} containers"),
        Ok(_) => {}
        Err(e) => tracing::error!("restoring container secrets: {e:#}"),
    }
    runtime::detect_gpu(&app).await;

    let port = |var: &str, default: u16| std::env::var(var).ok().and_then(|p| p.parse().ok()).unwrap_or(default);
    tokio::spawn(proxy::watch(app.clone()));
    tokio::spawn(acme::run(app.clone()));
    tokio::spawn(logs::collect(app.clone()));
    tokio::spawn(metrics::sample_loop(app.clone()));
    tokio::spawn(alerts::watch_events(app.clone()));
    tokio::spawn(cleanup::run(app.clone()));
    tokio::spawn(autoupdate::run(app.clone()));
    tokio::spawn(backup::scheduler(app.clone()));
    {
        let app = app.clone();
        let (http, https) = (port("TUGPORT_HTTP_PORT", 80), port("TUGPORT_HTTPS_PORT", 443));
        tokio::spawn(async move {
            if let Err(e) = proxy::serve(app, http, https).await {
                tracing::error!("proxy stopped: {e:#}");
            }
        });
    }

    let listen = app.settings().panel_listen;
    let router = api::router(app.clone());
    if let Some(legacy) = app.settings().legacy_listen {
        let router = router.clone();
        tokio::spawn(async move {
            match tokio::net::TcpListener::bind(&legacy).await {
                Ok(l) => {
                    tracing::info!("also serving the API on {legacy}");
                    let _ = axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
                }
                Err(e) => tracing::warn!("cannot listen on {legacy}: {e}"),
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("listening on {listen}"))?;
    tracing::info!("tugport {VERSION} panel on http://{listen}");
    axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
