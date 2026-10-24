//! Running helper programs (git, docker buildx, railpack...) with their output streamed to a log.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

fn program(cmd: &std::process::Command) -> String {
    cmd.get_program().to_string_lossy().to_string()
}

fn cannot_run(program: &str, e: std::io::Error) -> anyhow::Error {
    anyhow!("cannot run {program}: {e}")
}

/// Stdout of a finished command, or its stderr as the error.
fn stdout(program: &str, out: std::process::Output) -> Result<String> {
    if !out.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Send every line of `pipe` to `tx`.
fn pump(pipe: impl AsyncRead + Unpin + Send + 'static, tx: tokio::sync::mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send(line);
        }
    });
}

/// Run to completion, sending every output line to `log`. Killed if the future is dropped
/// (a cancelled deploy). Errors include the last lines of output.
pub async fn run_streamed(mut cmd: Command, log: &(dyn Fn(&str) + Sync)) -> Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true);
    let program = program(cmd.as_std());
    let mut child = cmd.spawn().map_err(|e| cannot_run(&program, e))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(p) = child.stdout.take() {
        pump(p, tx.clone());
    }
    if let Some(p) = child.stderr.take() {
        pump(p, tx);
    }
    let mut tail = std::collections::VecDeque::with_capacity(4);
    while let Some(line) = rx.recv().await {
        log(&line);
        tail.push_back(line);
        if tail.len() > 3 {
            tail.pop_front();
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        bail!("{program} exited with {}: {}", status.code().map_or("a signal".into(), |c| c.to_string()), Vec::from(tail).join(" | "));
    }
    Ok(())
}

/// Run and capture stdout; stderr goes into the error.
pub async fn output(mut cmd: Command) -> Result<String> {
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    let program = program(cmd.as_std());
    stdout(&program, cmd.output().await.map_err(|e| cannot_run(&program, e))?)
}

/// Run a short blocking command and capture its trimmed stdout; stderr goes into the error.
pub fn run_sync(cmd: &mut std::process::Command) -> Result<String> {
    let program = program(cmd);
    Ok(stdout(&program, cmd.stdin(Stdio::null()).output().map_err(|e| cannot_run(&program, e))?)?.trim().to_string())
}

/// Whether a blocking command runs and exits successfully, its output discarded.
pub fn succeeds(cmd: &mut std::process::Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
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
