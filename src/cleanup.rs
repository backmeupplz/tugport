//! Automatic Docker cleanup (on by default): leftover containers, images no deploy can
//! roll back to, dangling layers and build cache over the cap. Never touches volumes.

use crate::app::{App, AppState};
use crate::{runtime, store, util};
use anyhow::{Context, Result};
use bollard::query_parameters::{ListContainersOptions, ListImagesOptions, RemoveImageOptions};
use std::collections::{HashMap, HashSet};

pub async fn run(app: AppState) {
    // Once at start too: a server that restarts often while short on disk still gets cleaned.
    if let Err(e) = tick(app.clone()).await {
        tracing::warn!("cleanup: {e:#}");
    }
    util::every(600, "cleanup", || tick(app.clone())).await
}

async fn tick(app: AppState) -> Result<()> {
    let s = app.settings().docker_cleanup;
    let last: i64 = app.db.state("cleanup:last").unwrap_or(0);
    if s.enabled && util::now() - last >= s.interval_hours.max(1) as i64 * 3600 {
        match clean(&app).await {
            Ok(r) => tracing::info!("docker cleanup: {r}"),
            Err(e) => tracing::warn!("docker cleanup failed: {e:#}"),
        }
        app.db.set_state("cleanup:last", &util::now().to_string())?;
    }
    crate::logs::cleanup(&app).context("log cleanup failed")?;
    Ok(())
}

/// Images of Tugport's deploys, and those a rollback may still need (the newest few per service).
struct Images {
    ours: HashSet<String>,
    keep: HashSet<String>,
}

async fn clean(app: &App) -> Result<String> {
    let keep_n: HashMap<String, u32> = store::services(app, None)?.into_iter().map(|s| (s.id, s.spec.rollout.keep_images.max(1))).collect();
    let mut images = Images { ours: HashSet::new(), keep: HashSet::new() };
    let mut kept: HashMap<String, u32> = HashMap::new();
    let deploys = app.db.rows("SELECT service_id, image FROM deploys WHERE image IS NOT NULL ORDER BY created_at DESC", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for (sid, img) in deploys {
        let Some(&n) = keep_n.get(&sid) else { continue };
        let count = kept.entry(sid).or_default();
        if *count < n {
            images.keep.insert(img.clone());
            *count += 1;
        }
        images.ours.insert(img);
    }
    let (mut containers, mut removed) = (0, 0);
    for (node, docker) in runtime::dockers(app) {
        match clean_on(app, &node, &docker, &images).await {
            Ok((c, i)) => {
                containers += c;
                removed += i;
            }
            Err(e) if node == "local" => return Err(e),
            Err(e) => tracing::warn!("docker cleanup on {node}: {e:#}"),
        }
    }
    // Builds only run here; older Docker calls the cap --keep-storage.
    let cap = format!("{}gb", app.settings().docker_cleanup.build_cache_gb);
    for flag in ["--max-used-space", "--keep-storage"] {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(["builder", "prune", "-f", flag, &cap]);
        if crate::proc::output(cmd).await.is_ok() {
            break;
        }
    }
    Ok(format!("removed {containers} leftover containers and {removed} unused images"))
}

/// Leftover containers and unused images on one server.
async fn clean_on(app: &App, node: &str, docker: &bollard::Docker, images: &Images) -> Result<(usize, usize)> {
    let mut removed_containers = 0;
    let mut removed_images = 0;
    let all = docker.list_containers(Some(ListContainersOptions { all: true, ..Default::default() })).await?;
    let in_use: HashSet<&str> = all.iter().filter_map(|c| c.image_id.as_deref()).collect();

    // Containers Tugport left behind (failed or cancelled deploys), not part of what's live.
    for c in &all {
        let Some(labels) = &c.labels else { continue };
        let (Some(service), Some(deploy)) = (labels.get(runtime::L_SERVICE), labels.get(runtime::L_DEPLOY)) else { continue };
        if labels.contains_key("com.docker.compose.project") {
            continue;
        }
        let stale = matches!(c.state.map(|s| s.to_string()).as_deref(), Some("exited" | "created" | "dead"));
        let old = util::now() - c.created.unwrap_or(0) > 3600;
        if stale && old && runtime::live_deploy(app, service).as_deref() != Some(deploy.as_str()) {
            runtime::remove(app, node, c.id.as_deref().unwrap_or_default(), &runtime::listed_name(c.names.clone())).await?;
            removed_containers += 1;
        }
    }

    for i in docker.list_images(Some(ListImagesOptions { all: false, ..Default::default() })).await? {
        let built_by_us = i.labels.contains_key(runtime::L_SERVICE);
        let known = i.repo_tags.iter().any(|t| images.ours.contains(t));
        if !(built_by_us || known) || in_use.contains(i.id.as_str()) {
            continue;
        }
        if i.repo_tags.iter().any(|t| images.keep.contains(t)) || util::now() - i.created < 3600 {
            continue;
        }
        for t in &i.repo_tags {
            if docker.remove_image(t, Some(RemoveImageOptions { force: false, noprune: false }), None).await.is_ok() {
                removed_images += 1;
            }
        }
    }

    // Dangling layers.
    let prune = HashMap::from([("dangling".to_string(), vec!["true".to_string()])]);
    let _ = docker.prune_images(Some(bollard::query_parameters::PruneImagesOptions { filters: Some(prune) })).await;
    Ok((removed_containers, removed_images))
}
