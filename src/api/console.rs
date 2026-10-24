//! Browser shell into a running container, over a WebSocket.

use super::service_or_404;
use super::{ApiError, ApiResult, Auth};
use crate::app::AppState;
use crate::auth::Role;
use crate::runtime;
use crate::store::Service;
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

/// Browsers cannot read why a WebSocket upgrade failed, so refusals after sign-in arrive
/// on the socket: one `{"error": ...}` message, then close code 4403.
pub async fn console(State(app): State<AppState>, a: Auth, Path(id): Path<String>, Query(q): Query<ConsoleQuery>, ws: WebSocketUpgrade) -> Response {
    let target = open_target(&app, &a, &id, q.container.as_deref()).await;
    ws.on_upgrade(move |mut socket| async move {
        let (docker, cid, compose) = match target {
            Ok(t) => t,
            Err(e) => {
                let _ = socket.send(Message::Text(serde_json::json!({ "error": e.message }).to_string().into())).await;
                let _ = socket.send(Message::Close(Some(axum::extract::ws::CloseFrame { code: 4403, reason: "refused".into() }))).await;
                return;
            }
        };
        if let Err(e) = run(docker, cid, compose, socket).await {
            tracing::debug!("console closed: {e:#}");
        }
    })
}

/// A running container of the service (the one named, or one of the live deploy) for a caller
/// who may read its environment. `what` and `to` word the refusals.
async fn running(app: &AppState, a: &Auth, id: &str, container: Option<&str>, what: &str, to: &str) -> ApiResult<(Service, runtime::Replica)> {
    let s = service_or_404(app, a, id, Role::Operator)?;
    if !a.0.can_read_secrets(&s.project_id) {
        return Err(ApiError::forbidden(format!("{what} can read the service's environment, so it needs the secrets:read grant")));
    }
    let candidates = match container {
        Some(_) => runtime::replicas(app, &s.id).await?,
        None => runtime::current(app, &s).await?,
    };
    let target = candidates
        .into_iter()
        .filter(|r| r.state == "running")
        .find(|r| container.is_none_or(|c| r.id.starts_with(c) || r.name == c))
        .ok_or_else(|| ApiError::bad_request(format!("no running container to {to}")))?;
    Ok((s, target))
}

/// `sh -c script` through Tugport's init, so it sees the service's secrets. Compose containers
/// don't start through it, so they get a plain shell.
fn shell(compose: bool, script: String) -> Vec<String> {
    let sh = ["sh".into(), "-c".into(), script];
    if compose { sh.into() } else { [crate::secrets::INIT_PATH.into(), "exec".into(), "--".into()].into_iter().chain(sh).collect() }
}

async fn open_target(app: &AppState, a: &Auth, id: &str, container: Option<&str>) -> ApiResult<(bollard::Docker, String, bool)> {
    let (s, target) = running(app, a, id, container, "a shell", "open a shell in").await?;
    app.audit(Some(&a.0), "console.open", Some(&s.project_id), Some(&s.id), Some(&target.name), None);
    Ok((app.docker_on(&target.node)?, target.id, s.kind == "compose"))
}

async fn run(docker: bollard::Docker, container: String, compose: bool, socket: WebSocket) -> anyhow::Result<()> {
    let cmd = shell(compose, "command -v bash >/dev/null 2>&1 && exec bash || exec sh".into());
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
                        let size = |i: usize, default| v["resize"][i].as_u64().and_then(|n| i32::try_from(n).ok()).unwrap_or(default);
                        let _ = docker.resize_exec(&exec_id, bollard::query_parameters::ResizeExecOptions { h: size(1, 24), w: size(0, 80) }).await;
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
    let (s, target) = running(&app, &a, &id, b.container.as_deref(), "running commands", "run the command in").await?;
    let masker = crate::secrets::Masker::for_service(&app, &s);
    app.audit(Some(&a.0), "service.exec", Some(&s.project_id), Some(&s.id), Some(&target.name), Some(&masker.mask(&b.command).chars().take(300).collect::<String>()));
    let cmd = shell(s.kind == "compose", b.command);
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
