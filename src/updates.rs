//! Self-update from GitHub releases: verified download, wait for running deploys, then a
//! zero-downtime re-exec (see handoff.rs).

use crate::app::AppState;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const REPO: &str = "backmeupplz/tugport";

#[derive(Serialize, Clone)]
pub struct Release {
    pub version: String,
    pub notes: String,
    #[serde(skip)]
    binary: String,
    #[serde(skip)]
    sums: String,
}

fn asset_name() -> String {
    format!("tugport-linux-{}", std::env::consts::ARCH)
}

pub async fn latest() -> Result<Release> {
    let client = reqwest::Client::builder().user_agent(concat!("tugport/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(30)).build()?;
    let v: serde_json::Value = client.get(format!("https://api.github.com/repos/{REPO}/releases/latest")).send().await?.error_for_status()?.json().await?;
    let version = v["tag_name"].as_str().ok_or_else(|| anyhow!("no release found"))?.trim_start_matches('v').to_string();
    let find = |name: &str| {
        v["assets"].as_array().into_iter().flatten().find(|a| a["name"].as_str() == Some(name)).and_then(|a| a["browser_download_url"].as_str()).map(String::from)
    };
    Ok(Release {
        notes: v["body"].as_str().unwrap_or_default().chars().take(4000).collect(),
        binary: find(&asset_name()).ok_or_else(|| anyhow!("release {version} has no {} build", asset_name()))?,
        sums: find("SHA256SUMS").ok_or_else(|| anyhow!("release {version} has no checksums"))?,
        version,
    })
}

pub fn newer(candidate: &str) -> bool {
    match (semver::Version::parse(candidate), semver::Version::parse(crate::VERSION)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

/// Download, verify and install the latest release, then hand over to it.
pub async fn apply(app: &AppState) -> Result<()> {
    let r = latest().await?;
    if !newer(&r.version) {
        bail!("Tugport {} is already the newest version", crate::VERSION);
    }
    let exe = crate::handoff::binary_path()?;
    let tmp = exe.with_extension("download");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?;
    let sums = client.get(&r.sums).send().await?.error_for_status()?.text().await?;
    let expected = sums
        .lines()
        .find_map(|l| l.split_whitespace().collect::<Vec<_>>().split_first().and_then(|(h, rest)| (rest.first() == Some(&asset_name().as_str())).then(|| h.to_string())))
        .ok_or_else(|| anyhow!("SHA256SUMS does not list {}", asset_name()))?;
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut hasher = Sha256::new();
    let mut stream = client.get(&r.binary).send().await?.error_for_status()?.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    if hex::encode(hasher.finalize()) != expected {
        let _ = std::fs::remove_file(&tmp);
        bail!("the downloaded binary does not match its checksum");
    }
    crate::paths::restrict(&tmp, 0o755)?;
    // A sanity check that the new binary runs at all before switching to it.
    let probe = tokio::process::Command::new(&tmp).arg("version").output().await.context("running the new binary")?;
    if !probe.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("the new binary does not start");
    }
    tracing::info!("update to {} downloaded; waiting for running deploys", r.version);
    for _ in 0..600 {
        if app.deployer.idle() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let _ = std::fs::copy(&exe, exe.with_extension("previous"));
    std::fs::rename(&tmp, &exe)?;
    app.audit(None, "tugport.update", None, None, Some(&r.version), None);
    crate::handoff::reexec(&exe, Duration::from_secs(30)).await
}

/// Install new releases automatically when enabled in settings.
pub async fn auto(app: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        if !app.settings().updates.auto {
            continue;
        }
        match latest().await {
            Ok(r) if newer(&r.version) => {
                if let Err(e) = apply(&app).await {
                    tracing::warn!("automatic update failed: {e:#}");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("update check failed: {e:#}"),
        }
    }
}
