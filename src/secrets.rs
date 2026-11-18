//! Getting secrets into containers without them touching disk or `docker inspect`.
//!
//! Each container gets its own JSON env file in /run/tugport (tmpfs), bind-mounted
//! read-only at /run/tugport/env.json, and starts through Tugport's init, which loads
//! it into the app's environment. No other container can see the file.

use crate::app::App;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use zeroize::Zeroizing;

pub const INIT_PATH: &str = "/run/tugport/init";
pub const ENV_PATH: &str = "/run/tugport/env.json";

fn write_readable(path: &std::path::Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o444).open(&tmp)?;
        f.write_all(data)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn materialize(app: &App, container: &str, env: &BTreeMap<String, Zeroizing<String>>) -> Result<PathBuf> {
    let path = app.paths.secrets().join(format!("{container}.json"));
    let plain: BTreeMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let json = Zeroizing::new(serde_json::to_vec(&plain)?);
    write_readable(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Mounted files with their contents decrypted into tmpfs. Returns (host path, target).
pub fn materialize_files(app: &App, container: &str, files: &[(String, Zeroizing<String>)]) -> Result<Vec<(PathBuf, String)>> {
    if files.is_empty() {
        return Ok(vec![]);
    }
    let dir = app.paths.files().join(container);
    std::fs::create_dir_all(&dir)?;
    crate::paths::restrict(&dir, 0o700)?;
    files
        .iter()
        .map(|(target, content)| {
            let host = dir.join(crate::util::sha256_hex(target));
            write_readable(&host, content.as_bytes())?;
            Ok((host, target.clone()))
        })
        .collect()
}

pub fn forget(app: &App, container: &str) {
    let _ = std::fs::remove_file(app.paths.secrets().join(format!("{container}.json")));
    let _ = std::fs::remove_dir_all(app.paths.files().join(container));
}

/// /run is wiped on reboot: write every managed container's files again. Docker restarts
/// containers before Tugport is up, so after a reboot those starts fail on the missing
/// mounts and Docker never retries them: start what should run here, as `nodes::restore`
/// does on other servers.
pub async fn rematerialize(app: &App) -> Result<usize> {
    let (mut n, mut started) = (0, 0);
    for s in crate::store::services(app, None)? {
        if s.kind == "compose" {
            continue;
        }
        let mut env = crate::store::resolved_env(app, &s)?;
        crate::runtime::add_port(app, &s, &mut env)?;
        let files = crate::store::files(app, &s.id)?;
        let live = crate::runtime::live_deploy(app, &s.id);
        let stopped = crate::nodes::stopped(app, &s.id);
        // Other servers keep their own; the panel sends them again after a reboot there.
        for r in crate::runtime::replicas(app, &s.id).await?.into_iter().filter(|r| r.node == "local") {
            materialize(app, &r.name, &env)?;
            materialize_files(app, &r.name, &files)?;
            n += 1;
            if should_start(&r.state, &r.deploy, live.as_deref(), stopped) {
                match crate::runtime::start(app, "local", &r.id).await {
                    Ok(()) => started += 1,
                    Err(e) => tracing::warn!("starting {} again: {e:#}", r.name),
                }
            }
        }
    }
    if started > 0 {
        tracing::info!("started {started} containers that could not start before their secrets were back");
    }
    Ok(n)
}

/// A container of the live deploy (any before the first deploy) that is down although
/// nobody stopped the service and it is not asleep.
fn should_start(state: &str, deploy: &str, live: Option<&str>, stopped: bool) -> bool {
    matches!(state, "exited" | "created") && !stopped && live.is_none_or(|l| l == deploy)
}

/// Replaces secret values with dots in anything shown to people or agents.
pub struct Masker {
    ac: Option<aho_corasick::AhoCorasick>,
}

impl Masker {
    pub fn new<'a>(values: impl Iterator<Item = &'a str>) -> Self {
        let pats: Vec<&str> = values.filter(|v| v.len() >= 6).collect();
        if pats.is_empty() {
            return Masker { ac: None };
        }
        let ac = aho_corasick::AhoCorasick::new(pats).inspect_err(|e| tracing::warn!("secrets cannot be masked: {e}")).ok();
        Masker { ac }
    }

    pub fn for_service(app: &App, s: &crate::store::Service) -> Self {
        match crate::store::resolved_env(app, s) {
            Ok(env) => Masker::new(env.values().map(|v| v.as_str())),
            Err(_) => Masker { ac: None },
        }
    }

    pub fn mask<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        match &self.ac {
            Some(ac) if ac.is_match(text) => {
                let mut out = String::with_capacity(text.len());
                ac.replace_all_with(text, &mut out, |_, _, out| {
                    out.push_str("••••••");
                    true
                });
                out.into()
            }
            _ => text.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_long_values_only() {
        let m = Masker::new(["supersecretvalue", "abc"].into_iter());
        assert_eq!(m.mask("token=supersecretvalue ok abc"), "token=•••••• ok abc");
        assert_eq!(m.mask("nothing here"), "nothing here");
    }

    #[test]
    fn starts_only_live_containers_nobody_stopped() {
        assert!(should_start("exited", "d1", Some("d1"), false));
        assert!(should_start("created", "d1", None, false));
        assert!(!should_start("running", "d1", Some("d1"), false));
        assert!(!should_start("exited", "d0", Some("d1"), false)); // replaced deploy
        assert!(!should_start("exited", "d1", Some("d1"), true)); // stopped or asleep
    }
}
