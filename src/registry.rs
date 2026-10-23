//! Minimal container-registry client: tag digests and tag lists, with Docker Hub and
//! private-registry token auth. Used by image auto-update.

use crate::app::App;
use anyhow::{Result, anyhow, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION, WWW_AUTHENTICATE};

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json";

/// (api host, repository path) for an image reference.
pub fn locate(image: &str) -> (String, String) {
    let (repo, _) = crate::runtime::split_image(image);
    let host = crate::runtime::registry_host(repo);
    if host == "docker.io" {
        let path = repo.trim_start_matches("docker.io/");
        let path = if path.contains('/') { path.to_string() } else { format!("library/{path}") };
        ("registry-1.docker.io".into(), path)
    } else {
        (host.to_string(), repo[host.len() + 1..].to_string())
    }
}

fn credentials(app: &App, image: &str) -> Option<(String, String)> {
    use rusqlite::OptionalExtension;
    let host = crate::runtime::registry_host(image);
    let row: Option<(String, Vec<u8>)> = app
        .db
        .conn()
        .query_row("SELECT username, password FROM registries WHERE host = ?1", [host], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .ok()
        .flatten();
    let (u, p) = row?;
    Some((u, app.keys.open_str(&format!("registry:{host}"), &p).ok()?.to_string()))
}

/// A client for one registry URL: HTTPS only, and never to this server, the mesh or cloud
/// metadata (the image name, the token realm and paging links all come from outside).
async fn client_for(url: &str) -> Result<reqwest::Client> {
    crate::proxy::outside_client(url, &["https"]).await.ok_or_else(|| anyhow!("{url} is not an address Tugport may ask"))
}

async fn send(app: &App, image: &str, method: reqwest::Method, url: &str) -> Result<reqwest::Response> {
    let client = client_for(url).await?;
    let creds = credentials(app, image);
    let first = client.request(method.clone(), url).header(ACCEPT, MANIFEST_TYPES).send().await?;
    if first.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(first);
    }
    let challenge = first.headers().get(WWW_AUTHENTICATE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let auth = if let Some(params) = challenge.strip_prefix("Bearer ") {
        let field = |k: &str| params.split(',').find_map(|p| p.trim().strip_prefix(&format!("{k}=")).map(|v| v.trim_matches('"').to_string()));
        let realm = field("realm").ok_or_else(|| anyhow!("registry sent no token realm"))?;
        let mut req = client_for(&realm).await?.get(&realm).query(&[("service", field("service").unwrap_or_default()), ("scope", field("scope").unwrap_or_default())]);
        if let Some((u, p)) = &creds {
            req = req.basic_auth(u, Some(p));
        }
        let tok: serde_json::Value = serde_json::from_slice(&crate::util::read_capped(req.send().await?.error_for_status()?, 1 << 20).await?)?;
        let token = tok["token"].as_str().or(tok["access_token"].as_str()).ok_or_else(|| anyhow!("registry gave no token"))?;
        format!("Bearer {token}")
    } else if let Some((u, p)) = &creds {
        format!("Basic {}", crate::crypto::b64(format!("{u}:{p}").as_bytes()))
    } else {
        bail!("the registry needs credentials; add them under registries");
    };
    Ok(client.request(method, url).header(ACCEPT, MANIFEST_TYPES).header(AUTHORIZATION, auth).send().await?)
}

/// Digest the tag points to right now.
pub async fn remote_digest(app: &App, image: &str) -> Result<String> {
    let (host, path) = locate(image);
    let (_, tag) = crate::runtime::split_image(image);
    let resp = send(app, image, reqwest::Method::HEAD, &format!("https://{host}/v2/{path}/manifests/{tag}")).await?.error_for_status()?;
    resp.headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .ok_or_else(|| anyhow!("the registry did not return a digest"))
}

pub async fn tags(app: &App, image: &str) -> Result<Vec<String>> {
    let (host, path) = locate(image);
    let mut url = format!("https://{host}/v2/{path}/tags/list?n=1000");
    let mut out = vec![];
    for _ in 0..20 {
        let resp = send(app, image, reqwest::Method::GET, &url).await?.error_for_status()?;
        let next = resp.headers().get("link").and_then(|v| v.to_str().ok()).and_then(|l| l.split(';').next()).map(|l| l.trim_matches(['<', '>', ' ']).to_string());
        let v: serde_json::Value = serde_json::from_slice(&crate::util::read_capped(resp, 8 << 20).await?)?;
        out.extend(v["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(String::from)));
        match next {
            Some(n) if n.starts_with('/') => url = format!("https://{host}{n}"),
            Some(n) => url = n,
            None => break,
        }
    }
    Ok(out)
}

/// Newest tag with the same major version and the same suffix shape (`1.29-alpine`).
pub fn newer_same_major(current: &str, tags: &[String]) -> Option<String> {
    let (cur_ver, suffix) = split_version(current)?;
    tags.iter()
        .filter_map(|t| {
            let (v, s) = split_version(t)?;
            (s == suffix && v.first() == cur_ver.first() && v.len() == cur_ver.len() && v > cur_ver).then(|| (v, t.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, t)| t)
}

fn split_version(tag: &str) -> Option<(Vec<u64>, String)> {
    let (ver, suffix) = match tag.split_once('-') {
        Some((v, s)) => (v, s.to_string()),
        None => (tag, String::new()),
    };
    let ver = ver.trim_start_matches('v');
    let parts: Option<Vec<u64>> = ver.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty()).map(|p| (p, suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_newer_same_major() {
        let tags: Vec<String> = ["1.29.1", "1.29.3", "1.30.0", "2.0.0", "1.29.3-alpine", "1.30.1-alpine", "latest"].iter().map(|s| s.to_string()).collect();
        assert_eq!(newer_same_major("1.29.1", &tags).as_deref(), Some("1.30.0"));
        assert_eq!(newer_same_major("1.29-alpine", &tags), None, "different precision");
        assert_eq!(newer_same_major("1.29.3-alpine", &tags).as_deref(), Some("1.30.1-alpine"));
        assert_eq!(newer_same_major("2.0.0", &tags), None);
    }

    #[test]
    fn locates_images() {
        assert_eq!(locate("nginx:1.29"), ("registry-1.docker.io".into(), "library/nginx".into()));
        assert_eq!(locate("ghcr.io/o/r:1"), ("ghcr.io".into(), "o/r".into()));
        assert_eq!(locate("grafana/grafana:12"), ("registry-1.docker.io".into(), "grafana/grafana".into()));
    }
}
