//! Helper programs downloaded on first use, pinned by version and checksum.

use crate::app::App;
use crate::util;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub enum Tool {
    Railpack,
    Nixpacks,
    Pack,
}

struct Release {
    name: &'static str,
    version: &'static str,
    url: &'static str,
    sha256: &'static str,
}

fn release(tool: Tool) -> Release {
    // (x86_64, aarch64) archives with their sha256.
    let (name, version, [x86, arm]) = match tool {
        Tool::Railpack => ("railpack", "0.43.0", [
            ("https://github.com/railwayapp/railpack/releases/download/v0.43.0/railpack-v0.43.0-x86_64-unknown-linux-musl.tar.gz", "248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb"),
            ("https://github.com/railwayapp/railpack/releases/download/v0.43.0/railpack-v0.43.0-arm64-unknown-linux-musl.tar.gz", "732ece14eb4f4676a43ebba19431f954edf7b130acb76159ef0f2ff86ac42ac9"),
        ]),
        Tool::Nixpacks => ("nixpacks", "1.41.0", [
            ("https://github.com/railwayapp/nixpacks/releases/download/v1.41.0/nixpacks-v1.41.0-x86_64-unknown-linux-musl.tar.gz", "0f55de7874507b9cf7502113120bd96f2ab6979f78d10eaf2eb2ade9207b3af6"),
            ("https://github.com/railwayapp/nixpacks/releases/download/v1.41.0/nixpacks-v1.41.0-aarch64-unknown-linux-musl.tar.gz", "912bd02dd2bb6f9c3a9ed965fe8a68b4aa318dc7a2546e2eca6f2806a894ba39"),
        ]),
        Tool::Pack => ("pack", "0.40.10", [
            ("https://github.com/buildpacks/pack/releases/download/v0.40.10/pack-v0.40.10-linux.tgz", "687c3616e21824a449b56d4f989613ca68c921c6cce670b4de4459b025233806"),
            ("https://github.com/buildpacks/pack/releases/download/v0.40.10/pack-v0.40.10-linux-arm64.tgz", "9a294abb0576fd95e19a009b3f9d5f5aa9fecdc2d7da7fa05e7e1e94f5085d77"),
        ]),
    };
    let (url, sha256) = if std::env::consts::ARCH == "aarch64" { arm } else { x86 };
    Release { name, version, url, sha256 }
}

/// Pinned by digest like the tools themselves: BuildKit runs it for every Railpack build.
pub const RAILPACK_FRONTEND: &str = "ghcr.io/railwayapp/railpack-frontend:v0.43.0@sha256:355ef61fae6f2551103cd8b10c710cb07167dd353d561f15c69b8d7ac1c676cc";

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Path to the tool, downloading and verifying it the first time.
pub async fn ensure(app: &App, tool: Tool, log: &(dyn Fn(&str) + Sync)) -> Result<PathBuf> {
    let r = release(tool);
    let dest = app.paths.tools().join(format!("{}-{}", r.name, r.version));
    if dest.exists() {
        return Ok(dest);
    }
    let _guard = LOCK.lock().await;
    if dest.exists() {
        return Ok(dest);
    }
    log(&format!("Downloading {} {}", r.name, r.version));
    let work = app.paths.tools().join(format!(".dl-{}", util::id()));
    std::fs::create_dir_all(&work)?;
    let result = async {
        let archive = work.join("archive.tgz");
        let resp = reqwest::Client::builder()
            .user_agent(concat!("tugport/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            // No overall limit (slow links still finish), but a stalled download gives up.
            .read_timeout(std::time::Duration::from_secs(60))
            .build()?.get(r.url).send().await?;
        let got = util::download(resp, &archive).await?;
        if got != r.sha256 {
            bail!("{} download checksum mismatch ({got})", r.name);
        }
        let mut tar = tokio::process::Command::new("tar");
        tar.arg("-xzf").arg(&archive).arg("-C").arg(&work);
        crate::proc::output(tar).await?;
        let found = find(&work, r.name).with_context(|| format!("{} not found in the archive", r.name))?;
        std::fs::rename(&found, &dest)?;
        crate::paths::restrict(&dest, 0o755)?;
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&work);
    result.map(|_| dest)
}

fn find(dir: &std::path::Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(f) = find(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|f| f == name) {
            return Some(p);
        }
    }
    None
}
