//! Helper programs downloaded on first use, pinned by version and checksum.

use crate::app::App;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

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

fn release(tool: Tool) -> Result<Release> {
    let arm = std::env::consts::ARCH == "aarch64";
    Ok(match (tool, arm) {
        (Tool::Railpack, false) => Release {
            name: "railpack",
            version: "0.43.0",
            url: "https://github.com/railwayapp/railpack/releases/download/v0.43.0/railpack-v0.43.0-x86_64-unknown-linux-musl.tar.gz",
            sha256: "248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb",
        },
        (Tool::Railpack, true) => Release {
            name: "railpack",
            version: "0.43.0",
            url: "https://github.com/railwayapp/railpack/releases/download/v0.43.0/railpack-v0.43.0-arm64-unknown-linux-musl.tar.gz",
            sha256: "732ece14eb4f4676a43ebba19431f954edf7b130acb76159ef0f2ff86ac42ac9",
        },
        (Tool::Nixpacks, false) => Release {
            name: "nixpacks",
            version: "1.41.0",
            url: "https://github.com/railwayapp/nixpacks/releases/download/v1.41.0/nixpacks-v1.41.0-x86_64-unknown-linux-musl.tar.gz",
            sha256: "0f55de7874507b9cf7502113120bd96f2ab6979f78d10eaf2eb2ade9207b3af6",
        },
        (Tool::Nixpacks, true) => Release {
            name: "nixpacks",
            version: "1.41.0",
            url: "https://github.com/railwayapp/nixpacks/releases/download/v1.41.0/nixpacks-v1.41.0-aarch64-unknown-linux-musl.tar.gz",
            sha256: "912bd02dd2bb6f9c3a9ed965fe8a68b4aa318dc7a2546e2eca6f2806a894ba39",
        },
        (Tool::Pack, false) => Release {
            name: "pack",
            version: "0.40.10",
            url: "https://github.com/buildpacks/pack/releases/download/v0.40.10/pack-v0.40.10-linux.tgz",
            sha256: "687c3616e21824a449b56d4f989613ca68c921c6cce670b4de4459b025233806",
        },
        (Tool::Pack, true) => Release {
            name: "pack",
            version: "0.40.10",
            url: "https://github.com/buildpacks/pack/releases/download/v0.40.10/pack-v0.40.10-linux-arm64.tgz",
            sha256: "9a294abb0576fd95e19a009b3f9d5f5aa9fecdc2d7da7fa05e7e1e94f5085d77",
        },
    })
}

pub fn railpack_frontend() -> &'static str {
    "ghcr.io/railwayapp/railpack-frontend:v0.43.0"
}

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Path to the tool, downloading and verifying it the first time.
pub async fn ensure(app: &App, tool: Tool, log: &(dyn Fn(&str) + Sync)) -> Result<PathBuf> {
    let r = release(tool)?;
    let dest = app.paths.tools().join(format!("{}-{}", r.name, r.version));
    if dest.exists() {
        return Ok(dest);
    }
    let _guard = LOCK.lock().await;
    if dest.exists() {
        return Ok(dest);
    }
    log(&format!("Downloading {} {}", r.name, r.version));
    let work = app.paths.tools().join(format!(".dl-{}", crate::util::id()));
    std::fs::create_dir_all(&work)?;
    let result = async {
        let archive = work.join("archive.tgz");
        let resp = reqwest::get(r.url).await?.error_for_status()?;
        let mut file = tokio::fs::File::create(&archive).await?;
        let mut hasher = Sha256::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        let got = hex::encode(hasher.finalize());
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
