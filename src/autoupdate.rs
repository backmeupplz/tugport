//! Image auto-update (off by default): services that run a registry image redeploy when
//! their tag moves, or move to a newer version with the same major number.

use crate::app::{App, AppState};
use crate::deploy::{self, Request};
use crate::model::{Source, UpdatePolicy};
use crate::{registry, runtime, store, util};
use anyhow::Result;

pub async fn run(app: AppState) {
    util::every(60, "auto-update", || tick(app.clone())).await
}

async fn tick(app: AppState) -> Result<()> {
    for mut s in store::services(&app, None)? {
        let au = &s.spec.auto_update;
        let (true, Source::Image { image }) = (au.enabled, &s.spec.source) else { continue };
        let key = format!("autoupdate:{}", s.id);
        let now = util::now();
        if now.saturating_sub(app.db.state(&key).unwrap_or(0)) < i64::from(au.interval_minutes.max(5)) * 60 {
            continue;
        }
        let _ = app.db.set_state(&key, &now.to_string());
        let (reason, new_image) = match check(&app, &s, image, au.policy).await {
            Ok(Some(found)) => found,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("auto-update check for {} failed: {e:#}", s.name);
                continue;
            }
        };
        if let Some(image) = new_image {
            s.spec.source = Source::Image { image };
            if let Err(e) = store::save_spec(&app, &s.id, &s.spec) {
                tracing::warn!("auto-update could not save {}: {e:#}", s.name);
                continue;
            }
        }
        let req = Request { trigger: "autoupdate".into(), actor: Some("auto-update".into()), ..Default::default() };
        match deploy::enqueue(&app, &s, req) {
            Ok(id) => {
                tracing::info!("auto-update: {}/{} {reason}", s.project, s.name);
                app.audit(None, "deploy.autoupdate", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&id));
            }
            Err(e) => tracing::warn!("auto-update deploy for {} failed to queue: {e:#}", s.name),
        }
    }
    Ok(())
}

/// Returns why to update and, for version bumps, the new image.
async fn check(app: &App, s: &store::Service, image: &str, policy: UpdatePolicy) -> Result<Option<(String, Option<String>)>> {
    match policy {
        UpdatePolicy::Digest => {
            let remote = registry::remote_digest(app, image).await?;
            let running = runtime::replicas(app, &s.id).await?.into_iter().next().map(|r| r.image);
            let local = app.docker.inspect_image(running.as_deref().unwrap_or(image)).await.ok().and_then(|i| i.repo_digests).unwrap_or_default();
            if local.iter().any(|d| d.ends_with(&remote)) {
                return Ok(None);
            }
            Ok(Some((format!("{image} has a new image ({})", remote.chars().take(19).collect::<String>()), None)))
        }
        UpdatePolicy::SameMajor => {
            let (repo, tag) = runtime::split_image(image);
            let tags = registry::tags(app, image).await?;
            Ok(registry::newer_same_major(tag, &tags).map(|t| (format!("{tag} -> {t}"), Some(format!("{repo}:{t}")))))
        }
    }
}
