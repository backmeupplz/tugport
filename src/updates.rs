//! Self-update from GitHub releases: verified download, wait for running deploys, then a
//! zero-downtime re-exec (see handoff.rs).

use crate::app::AppState;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::time::Duration;

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
    let client = crate::util::http(30).https_only(true).build()?;
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

/// Whether release `candidate` (`x.y.z`) is newer than this build. Anything else, such as a
/// pre-release, never is.
pub fn newer(candidate: &str) -> bool {
    fn parse(v: &str) -> Option<[u64; 3]> {
        let mut parts = v.split('.').map(|p| p.parse().ok());
        let v = [parts.next()??, parts.next()??, parts.next()??];
        parts.next().is_none().then_some(v)
    }
    matches!((parse(candidate), parse(crate::VERSION)), (Some(a), Some(b)) if a > b)
}

/// The hash `SHA256SUMS` lists for `name` (lines of `<hash>  <name>`).
fn checksum<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        let hash = words.next()?;
        (words.next()? == name).then_some(hash)
    })
}

/// Download, verify and install the latest release, then hand over to it.
pub async fn apply(app: &AppState) -> Result<()> {
    install(app, latest().await?).await
}

async fn install(app: &AppState, r: Release) -> Result<()> {
    if !newer(&r.version) {
        bail!("Tugport {} is already the newest version", crate::VERSION);
    }
    let exe = crate::handoff::binary_path()?;
    let tmp = exe.with_extension("download");
    let client = crate::util::http(600).https_only(true).build()?;
    let sums = client.get(&r.sums).send().await?.error_for_status()?.text().await?;
    let asset = asset_name();
    let expected = checksum(&sums, &asset).ok_or_else(|| anyhow!("SHA256SUMS does not list {asset}"))?;
    if !crate::util::download(client.get(&r.binary).send().await?, &tmp).await?.eq_ignore_ascii_case(expected) {
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
    crate::util::every(6 * 3600, "automatic update", || auto_tick(app.clone())).await
}

async fn auto_tick(app: AppState) -> Result<()> {
    if app.settings().updates.auto {
        let r = latest().await?;
        if newer(&r.version) {
            install(&app, r).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_and_versions() {
        let sums = "aa  tugport-linux-aarch64\nbb *tugport-linux-x86_64\ncc  tugport-linux-x86_64.sig\n\n";
        assert_eq!(checksum(sums, "tugport-linux-aarch64"), Some("aa"));
        assert_eq!(checksum(sums, "tugport-linux-x86_64"), None, "binary-mode entries keep their star");
        assert_eq!(checksum(sums, "tugport-linux-x86_64.sig"), Some("cc"));
        assert!(newer("999.0.0"));
        assert!(!newer("0.0.1") && !newer("999.0.0-rc1") && !newer("999.0"));
    }
}
