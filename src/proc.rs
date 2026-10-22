//! Running helper programs (git, docker buildx, railpack...) with their output streamed to a log.

use anyhow::{Result, bail};
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
