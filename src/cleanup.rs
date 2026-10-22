//! Automatic Docker cleanup (on by default): leftover containers, images no deploy can
//! roll back to, dangling layers and build cache over the cap. Never touches volumes.

use crate::app::{App, AppState};
use crate::{runtime, store, util};
use anyhow::Result;
use bollard::query_parameters::{ListContainersOptions, ListImagesOptions, RemoveImageOptions};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub async fn run(app: AppState) {
    loop {
        let s = app.settings().docker_cleanup;
        let last: i64 = app.db.get_state("cleanup:last").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
        if s.enabled && util::now() - last >= s.interval_hours.max(1) as i64 * 3600 {
            match clean(&app).await {
                Ok(r) => tracing::info!("docker cleanup: {r}"),
                Err(e) => tracing::warn!("docker cleanup failed: {e:#}"),
            }
            let _ = app.db.set_state("cleanup:last", &util::now().to_string());
        }
        if let Err(e) = crate::logs::cleanup(&app) {
            tracing::warn!("log cleanup failed: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}

pub async fn clean(app: &App) -> Result<String> {
    let (mut containers, mut images) = (0, 0);
    for (node, docker) in runtime::dockers(app) {
        match clean_on(app, &node, &docker).await {
            Ok((c, i)) => {
                containers += c;
                images += i;
            }
            Err(e) if node == "local" => return Err(e),
            Err(e) => tracing::warn!("docker cleanup on {node}: {e:#}"),
        }
    }
    crate::nodes::reset_registry(app).await;
    // Builds only run here.
    let cap = format!("{}gb", app.settings().docker_cleanup.build_cache_gb);
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(["builder", "prune", "-f", "--max-used-space", &cap]);
    if crate::proc::output(cmd).await.is_err() {
        let mut old = tokio::process::Command::new("docker");
        old.args(["builder", "prune", "-f", "--keep-storage", &cap]);
        let _ = crate::proc::output(old).await;
    }
    Ok(format!("removed {containers} leftover containers and {images} unused images"))
}

/// Leftover containers and unused images on one server.
async fn clean_on(app: &App, node: &str, docker: &bollard::Docker) -> Result<(usize, usize)> {
    let mut removed_containers = 0;
    let mut removed_images = 0;
    let all = docker.list_containers(Some(ListContainersOptions { all: true, ..Default::default() })).await?;
    let in_use: HashSet<String> = all.iter().filter_map(|c| c.image_id.clone()).collect();

    // Containers Tugport left behind (failed or cancelled deploys), not part of what's live.
    for c in &all {
        let labels = c.labels.clone().unwrap_or_default();
        let (Some(service), Some(deploy)) = (labels.get(runtime::L_SERVICE), labels.get(runtime::L_DEPLOY)) else { continue };
        if labels.contains_key("com.docker.compose.project") {
            continue;
        }
        let stale = matches!(c.state.map(|s| s.to_string()).as_deref(), Some("exited" | "created" | "dead"));
        let old = util::now() - c.created.unwrap_or(0) > 3600;
        if stale && old && runtime::live_deploy(app, service).as_deref() != Some(deploy.as_str()) {
            let name = c.names.as_ref().and_then(|n| n.first()).map(|n| n.trim_start_matches('/').to_string()).unwrap_or_default();
            runtime::remove(app, node, c.id.as_deref().unwrap_or_default(), &name).await?;
            removed_containers += 1;
        }
    }

    // Images that a rollback may still need: the newest few successful deploys per service.
    let mut keep: HashSet<String> = HashSet::new();
    let mut ours: HashSet<String> = HashSet::new();
    for s in store::services(app, None)? {
        let n = s.spec.rollout.keep_images.max(1);
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT image FROM deploys WHERE service_id = ?1 AND image IS NOT NULL ORDER BY created_at DESC")?;
        let images: Vec<String> = stmt.query_map([&s.id], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
        let mut live_seen = 0;
        for img in images {
            ours.insert(img.clone());
            if live_seen < n {
                keep.insert(img);
                live_seen += 1;
            }
        }
    }
    let images = docker.list_images(Some(ListImagesOptions { all: false, ..Default::default() })).await?;
    let mut by_tag: HashMap<String, (String, i64)> = HashMap::new();
    for i in &images {
        for t in i.repo_tags.iter() {
            by_tag.insert(t.clone(), (i.id.clone(), i.created));
        }
    }
    for i in &images {
        let built_by_us = i.labels.contains_key(runtime::L_SERVICE);
        let tags: Vec<&String> = i.repo_tags.iter().collect();
        let known = tags.iter().any(|t| ours.contains(*t));
        if !(built_by_us || known) || in_use.contains(&i.id) {
            continue;
        }
        if tags.iter().any(|t| keep.contains(*t)) || util::now() - i.created < 3600 {
            continue;
        }
        for t in &tags {
            if docker.remove_image(t, Some(RemoveImageOptions { force: false, noprune: false }), None).await.is_ok() {
                removed_images += 1;
            }
        }
    }

    // Dangling layers.
    let mut prune = HashMap::new();
    prune.insert("dangling".to_string(), vec!["true".to_string()]);
    let _ = docker.prune_images(Some(bollard::query_parameters::PruneImagesOptions { filters: Some(prune) })).await;
    Ok((removed_containers, removed_images))
}
