//! Image auto-update (off by default): services that run a registry image redeploy when
//! their tag moves, or move to a newer version with the same major number.

use crate::app::{App, AppState};
use crate::deploy::{self, Request};
use crate::model::{Source, UpdatePolicy};
use crate::{registry, runtime, store, util};
use anyhow::Result;
use std::time::Duration;

pub async fn run(app: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let Ok(services) = store::services(&app, None) else { continue };
        for s in services {
            let au = &s.spec.auto_update;
            let Source::Image { image } = &s.spec.source else { continue };
            if !au.enabled {
                continue;
            }
            let key = format!("autoupdate:{}", s.id);
            let last: i64 = app.db.get_state(&key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
            if util::now() - last < au.interval_minutes.max(5) as i64 * 60 {
                continue;
            }
            let _ = app.db.set_state(&key, &util::now().to_string());
            match check(&app, &s, image, au.policy).await {
                Ok(Some((reason, new_image))) => {
                    if let Some(img) = &new_image {
                        let mut spec = s.spec.clone();
                        spec.source = Source::Image { image: img.clone() };
                        if let Err(e) = store::save_spec(&app, &s.id, &spec) {
                            tracing::warn!("auto-update could not save {}: {e:#}", s.name);
                            continue;
                        }
                    }
                    let Ok(Some(fresh)) = store::service(&app, &s.id) else { continue };
                    let req = Request { trigger: "autoupdate".into(), actor: Some("auto-update".into()), ..Default::default() };
                    match deploy::enqueue(&app, &fresh, req) {
                        Ok(id) => {
                            tracing::info!("auto-update: {}/{} {reason}", s.project, s.name);
                            app.audit(None, "deploy.autoupdate", Some(&s.project_id), Some(&s.id), Some(&s.name), Some(&id));
                        }
                        Err(e) => tracing::warn!("auto-update deploy for {} failed to queue: {e:#}", s.name),
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("auto-update check for {} failed: {e:#}", s.name),
            }
        }
    }
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
            Ok(Some((format!("{image} has a new image ({})", &remote[..remote.len().min(19)]), None)))
        }
        UpdatePolicy::SameMajor => {
            let (repo, tag) = runtime::split_image(image);
            let tags = registry::tags(app, image).await?;
            Ok(registry::newer_same_major(tag, &tags).map(|t| (format!("{tag} -> {t}"), Some(format!("{repo}:{t}")))))
        }
    }
}
