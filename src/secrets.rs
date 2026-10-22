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
    let dir = app.paths.run.join("files").join(container);
    std::fs::create_dir_all(&dir)?;
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
    let _ = std::fs::remove_dir_all(app.paths.run.join("files").join(container));
}

/// /run is wiped on reboot: write every managed container's files again before
/// Docker restarts them.
pub async fn rematerialize(app: &App) -> Result<usize> {
    let mut n = 0;
    for s in crate::store::services(app, None)? {
        if s.kind == "compose" {
            continue;
        }
        let env = crate::store::resolved_env(app, &s)?;
        let files = crate::store::files(app, &s.id)?;
        for r in crate::runtime::replicas(app, &s.id).await? {
            materialize(app, &r.name, &env)?;
            materialize_files(app, &r.name, &files)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Replaces secret values with dots in anything shown to people or agents.
pub struct Masker {
    ac: Option<aho_corasick::AhoCorasick>,
}

impl Masker {
    pub fn new<'a>(values: impl Iterator<Item = &'a str>) -> Self {
        let pats: Vec<&str> = values.filter(|v| v.len() >= 6).collect();
        Masker { ac: (!pats.is_empty()).then(|| aho_corasick::AhoCorasick::new(pats).ok()).flatten() }
    }

    pub fn for_service(app: &App, s: &crate::store::Service) -> Self {
        match crate::store::resolved_env(app, s) {
            Ok(env) => Masker::new(env.values().map(|v| v.as_str())),
            Err(_) => Masker { ac: None },
        }
    }

    pub fn mask<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        match &self.ac {
            Some(ac) if ac.is_match(text) => ac.replace_all(text, &vec!["••••••"; ac.patterns_len()]).into(),
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
}
