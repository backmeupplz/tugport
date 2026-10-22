//! Browser shell into a running container, over a WebSocket.

use super::projects::service_or_404;
use super::{ApiError, ApiResult, Auth};
use crate::app::AppState;
use crate::auth::Role;
use crate::runtime;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::Response;
use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::ExecConfig;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ConsoleQuery {
    container: Option<String>,
}

pub async fn console(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Query(q): Query<ConsoleQuery>, ws: WebSocketUpgrade) -> ApiResult<Response> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if !a.0.can_read_secrets(&s.project_id) {
        return Err(ApiError::forbidden("a shell can read the service's environment, so it needs the secrets:read grant"));
    }
    let live = runtime::live_deploy(&app, &s.id);
    let reps = runtime::replicas(&app, &s.id).await?;
    let target = reps
        .iter()
        .filter(|r| r.state == "running")
        .find(|r| match &q.container {
            Some(c) => r.id.starts_with(c.as_str()) || r.name == *c,
            None => s.kind == "compose" || live.as_deref().is_none_or(|l| r.deploy == l),
        })
        .ok_or_else(|| ApiError::bad_request("no running container to open a shell in"))?;
    app.audit(Some(&a.0), "console.open", Some(&s.project_id), Some(&s.id), Some(&target.name), None);
    let docker = app.docker_on(&target.node)?;
    let (cid, compose) = (target.id.clone(), s.kind == "compose");
    Ok(ws.on_upgrade(move |socket| async move {
        if let Err(e) = run(docker, cid, compose, socket).await {
            tracing::debug!("console closed: {e:#}");
        }
    }))
}

async fn run(docker: bollard::Docker, container: String, compose: bool, socket: WebSocket) -> anyhow::Result<()> {
    let shell = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";
    // Compose containers don't start through Tugport's init, so they get a plain shell.
    let cmd: Vec<String> = if compose {
        vec!["sh".into(), "-c".into(), shell.into()]
    } else {
        vec![crate::secrets::INIT_PATH.into(), "exec".into(), "--".into(), "sh".into(), "-c".into(), shell.into()]
    };
    let exec = docker
        .create_exec(
            &container,
            ExecConfig {
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                tty: Some(true),
                env: Some(vec!["TERM=xterm-256color".into()]),
                cmd: Some(cmd),
                ..Default::default()
            },
        )
        .await?;
    let StartExecResults::Attached { mut output, mut input } =
        docker.start_exec(&exec.id, Some(StartExecOptions { detach: false, tty: true, output_capacity: None })).await?
    else {
        anyhow::bail!("exec detached");
    };
    let (mut tx, mut rx) = socket.split();
    let to_browser = async {
        while let Some(Ok(chunk)) = output.next().await {
            if tx.send(Message::Binary(chunk.into_bytes())).await.is_err() {
                break;
            }
        }
        let _ = tx.close().await;
    };
    let exec_id = exec.id.clone();
    let from_browser = async {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Text(t) if t.as_str().starts_with("{\"resize\"") => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t.as_str()) {
                        let (cols, rows) = (v["resize"][0].as_u64().unwrap_or(80), v["resize"][1].as_u64().unwrap_or(24));
                        let _ = docker
                            .resize_exec(&exec_id, bollard::query_parameters::ResizeExecOptions { h: rows as i32, w: cols as i32 })
                            .await;
                    }
                }
                Message::Text(t) => {
                    if input.write_all(t.as_str().as_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Binary(b) => {
                    if input.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = to_browser => {}
        _ = from_browser => {}
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct ExecBody {
    command: String,
    timeout_s: Option<u64>,
    container: Option<String>,
}

/// Run one command in a running container and return its output (for agents and the CLI).
pub async fn exec(State(app): State<AppState>, a: Auth, Path(id): Path<String>, axum::Json(b): axum::Json<ExecBody>) -> ApiResult<axum::Json<serde_json::Value>> {
    let s = service_or_404(&app, &a, &id, Role::Operator)?;
    if !a.0.can_read_secrets(&s.project_id) {
        return Err(ApiError::forbidden("running commands can read the service's environment, so it needs the secrets:read grant"));
    }
    let live = runtime::live_deploy(&app, &s.id);
    let reps = runtime::replicas(&app, &s.id).await?;
    let target = reps
        .iter()
        .filter(|r| r.state == "running")
        .find(|r| match &b.container {
            Some(c) => r.id.starts_with(c.as_str()) || r.name == *c,
            None => s.kind == "compose" || live.as_deref().is_none_or(|l| r.deploy == l),
        })
        .ok_or_else(|| ApiError::bad_request("no running container to run the command in"))?;
    let masker = crate::secrets::Masker::for_service(&app, &s);
    app.audit(Some(&a.0), "service.exec", Some(&s.project_id), Some(&s.id), Some(&target.name), Some(&masker.mask(&b.command).chars().take(300).collect::<String>()));
    let cmd: Vec<String> = if s.kind == "compose" {
        vec!["sh".into(), "-c".into(), b.command.clone()]
    } else {
        vec![crate::secrets::INIT_PATH.into(), "exec".into(), "--".into(), "sh".into(), "-c".into(), b.command.clone()]
    };
    let docker = app.docker_on(&target.node)?;
    let exec = docker
        .create_exec(&target.id, ExecConfig { attach_stdout: Some(true), attach_stderr: Some(true), cmd: Some(cmd), ..Default::default() })
        .await
        .map_err(anyhow::Error::from)?;
    let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await.map_err(anyhow::Error::from)? else {
        return Err(ApiError::bad_request("the command detached"));
    };
    let mut text = String::new();
    let timeout = std::time::Duration::from_secs(b.timeout_s.unwrap_or(60).min(3600));
    let timed_out = tokio::time::timeout(timeout, async {
        while let Some(Ok(chunk)) = output.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk.into_bytes()));
            if text.len() > 1_000_000 {
                text.push_str("\n[output truncated]");
                break;
            }
        }
    })
    .await
    .is_err();
    let code = docker.inspect_exec(&exec.id).await.ok().and_then(|i| i.exit_code);
    Ok(axum::Json(serde_json::json!({ "exit_code": code, "timed_out": timed_out, "output": text })))
}
