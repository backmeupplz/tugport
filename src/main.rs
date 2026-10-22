mod api;
mod app;
mod auth;
mod crypto;
mod db;
mod model;
mod paths;
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

async fn serve() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "tugport=info".into()))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let paths = paths::Paths::from_env();
    paths.ensure().with_context(|| format!("creating {} and {}", paths.data.display(), paths.etc.display()))?;
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

    let (events, _) = tokio::sync::broadcast::channel(1024);
    let app: app::AppState = Arc::new(app::App { paths, db, keys: crypto::Keyring::new(&master), docker, events });
    drop(master);

    let listen = app.settings().panel_listen;
    let router = api::router(app.clone());
    let listener = tokio::net::TcpListener::bind(&listen).await.with_context(|| format!("listening on {listen}"))?;
    tracing::info!("tugport {VERSION} panel on http://{listen}");
    axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
