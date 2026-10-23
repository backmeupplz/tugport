//! Running helper programs (git, docker buildx, railpack...) with their output streamed to a log.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Run to completion, sending every output line to `log`. Killed if the future is dropped
/// (a cancelled deploy). Errors include the last lines of output.
pub async fn run_streamed(mut cmd: Command, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
    let program = cmd.as_std().get_program().to_string_lossy().to_string();
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("cannot run {program}: {e}"))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|p| Box::new(p) as _)]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut tail: std::collections::VecDeque<String> = Default::default();
    while let Some(line) = rx.recv().await {
        log(&line);
        tail.push_back(line);
        if tail.len() > 15 {
            tail.pop_front();
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        let last: Vec<String> = tail.into_iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("{program} exited with {}: {}", status.code().map(|c| c.to_string()).unwrap_or("a signal".into()), last.join(" | "));
    }
    Ok(())
}

/// Run and capture stdout; stderr goes into the error.
pub async fn output(mut cmd: Command) -> Result<String> {
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    let program = cmd.as_std().get_program().to_string_lossy().to_string();
    let out = cmd.output().await.map_err(|e| anyhow::anyhow!("cannot run {program}: {e}"))?;
    if !out.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Variable names that change how the host tools Tugport runs (docker, compose, buildx, git,
/// railpack...) behave, which binaries and libraries they load or which config and credentials
/// they read, rather than the build or stack they work on. Services may still set them for their
/// containers; they just never reach those tools' own environment.
pub fn steers_tools(name: &str) -> bool {
    const EXACT: &[&str] =
        &["PATH", "HOME", "SHELL", "ENV", "BASH_ENV", "IFS", "TMPDIR", "GCONV_PATH", "GLIBC_TUNABLES", "HOSTALIASES", "LOCPATH", "NLSPATH", "SSL_CERT_FILE", "SSL_CERT_DIR"];
    const PREFIXES: &[&str] = &["LD_", "DOCKER_", "BUILDX_", "BUILDKIT_", "COMPOSE_", "GIT_", "SSH_", "XDG_", "MALLOC_"];
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// A service's variables that may go into a host tool's environment.
pub fn tool_env(env: &BTreeMap<String, zeroize::Zeroizing<String>>) -> impl Iterator<Item = (&str, &str)> {
    env.iter().filter(|(k, _)| !steers_tools(k)).map(|(k, v)| (k.as_str(), v.as_str()))
}

#[cfg(test)]
mod tests {
    use super::steers_tools;

    #[test]
    fn tool_variables() {
        for k in ["PATH", "LD_PRELOAD", "DOCKER_CONFIG", "DOCKER_HOST", "COMPOSE_FILE", "GIT_SSH_COMMAND", "HOME"] {
            assert!(steers_tools(k), "{k}");
        }
        for k in ["DATABASE_URL", "NODE_ENV", "PORT", "RAILPACK_NODE_VERSION", "NIXPACKS_PKGS", "MY_PATH"] {
            assert!(!steers_tools(k), "{k}");
        }
    }
}
