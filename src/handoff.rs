//! Zero-downtime restarts. Listening sockets are kept open across a re-exec of the new
//! binary (same PID, so systemd sees nothing); while the new process starts, the kernel
//! queues incoming connections instead of refusing them.

use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering, fence};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const ENV: &str = "TUGPORT_LISTEN_FDS";

/// Cancelled when a handoff starts: connections from before it finish their requests and close.
static DRAIN: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);
/// Cancelled right before the exec: listeners stop accepting, and connections accepted during
/// the drain finish their requests and close too.
static STOP: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);
/// Connections in flight: from before the drain, and accepted during it.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static LATE: AtomicUsize = AtomicUsize::new(0);
/// Accept loops running: one past STOP may still be handing a connection over.
static ACCEPTING: AtomicUsize = AtomicUsize::new(0);
/// How long connections accepted during the drain get once accepting stops. New ones wait in
/// the kernel's queue meanwhile, so this stays short.
const LATE_GRACE: Duration = Duration::from_secs(5);
static FDS: Mutex<Vec<(String, RawFd)>> = Mutex::new(Vec::new());

pub fn draining() -> bool {
    DRAIN.is_cancelled()
}

/// Resolves once a handoff starts draining.
pub async fn drained() {
    DRAIN.cancelled().await
}

/// Resolves once accepting stops, right before the exec.
pub async fn stopped() {
    STOP.cancelled().await
}

/// Counts a connection as in flight while alive.
pub struct ConnGuard(&'static AtomicUsize);

impl ConnGuard {
    pub fn new() -> Self {
        // Counted before the check: a connection the drain's first wait can miss sees the drain
        // and moves to LATE, which the last wait covers.
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if !draining() {
            return ConnGuard(&ACTIVE);
        }
        LATE.fetch_add(1, Ordering::SeqCst);
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
        ConnGuard(&LATE)
    }

    /// Resolves when the connection should finish its requests and close: when a handoff
    /// starts for connections from before it, right before the exec for ones accepted since.
    pub async fn closing(&self) {
        if std::ptr::eq(self.0, &ACTIVE) { DRAIN.cancelled().await } else { STOP.cancelled().await }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Hand each connection to `on` until the process stops accepting for a handoff.
pub async fn accept(listener: tokio::net::TcpListener, on: impl Fn(tokio::net::TcpStream, std::net::SocketAddr)) {
    ACCEPTING.fetch_add(1, Ordering::SeqCst);
    let _running = ConnGuard(&ACCEPTING);
    while !STOP.is_cancelled() {
        let accepted = tokio::select! {
            biased;
            _ = STOP.cancelled() => break,
            r = listener.accept() => r,
        };
        match accepted {
            Ok((stream, peer)) => on(stream, peer),
            // Out of file descriptors, say: back off instead of spinning.
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

fn inherited(key: &str) -> Option<RawFd> {
    std::env::var(ENV).ok()?.split(',').find_map(|pair| {
        let (k, fd) = pair.split_once('=')?;
        if k == key { fd.parse().ok() } else { None }
    })
}

/// A listener for `key` ("proxy:80", "panel"): the one inherited from the previous process
/// if there is one, otherwise a fresh one from `bind`.
pub async fn listener<F>(key: &str, bind: F) -> Result<tokio::net::TcpListener>
where
    F: std::future::Future<Output = Result<tokio::net::TcpListener>>,
{
    let l = match inherited(key) {
        Some(fd) => {
            // SAFETY: the previous process handed us this open listening socket under `key`
            // and nothing else in this process owns the descriptor.
            let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
            // Inherited without close-on-exec; programs Tugport runs (git, docker...) must not get it.
            // SAFETY: fcntl on the descriptor we now own.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            std_listener.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(std_listener).with_context(|| format!("adopting the inherited {key} socket"))?
        }
        None => bind.await?,
    };
    // A duplicate keeps the socket open for the next handoff; `reexec` lets it through the exec.
    // SAFETY: dup on a valid descriptor we own; the result is only stored, never closed twice.
    let dup = unsafe { libc::fcntl(l.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup >= 0 {
        crate::util::lock(&FDS).push((key.to_string(), dup));
    }
    Ok(l)
}

/// The binary on disk. After it is replaced, Linux reports our own path with a
/// " (deleted)" suffix; the new file lives at the plain path.
pub fn binary_path() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let text = exe.to_string_lossy();
    Ok(match text.strip_suffix(" (deleted)") {
        Some(plain) => std::path::PathBuf::from(plain),
        None => exe,
    })
}

/// Let in-flight connections finish (up to `grace`), stop accepting, then exec `binary`
/// with the same arguments, passing the listening sockets along. The binary is tried
/// first, so a broken one leaves this process serving.
pub async fn reexec(binary: &std::path::Path, grace: Duration) -> Result<()> {
    let probe = tokio::time::timeout(Duration::from_secs(10), tokio::process::Command::new(binary).arg("version").output()).await;
    match probe {
        Ok(Ok(out)) if out.status.success() => {}
        _ => bail!("{} does not run; staying on the current version", binary.display()),
    }
    drain(grace).await;
    let held = crate::util::lock(&FDS);
    for (_, fd) in held.iter() {
        // SAFETY: clearing close-on-exec on descriptors this process keeps for the new binary.
        unsafe { libc::fcntl(*fd, libc::F_SETFD, 0) };
    }
    tracing::info!("handing {} sockets to {}", held.len(), binary.display());
    let fds = held.iter().map(|(k, fd)| format!("{k}={fd}")).collect::<Vec<_>>().join(",");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(binary).args(std::env::args_os().skip(1)).env(ENV, fds).exec();
    // exec only returns on failure. Listeners have stopped, so exit and let the
    // service manager start the process again rather than linger without serving.
    tracing::error!("could not start {}: {err}; exiting so the service restarts", binary.display());
    std::process::exit(1)
}

/// Keep accepting while the connections from before the drain finish, so visitors are queued
/// only for the exec and not for the whole grace (a stream that never ends used up all of it).
async fn drain(grace: Duration) {
    DRAIN.cancel();
    fence(Ordering::SeqCst);
    settle(&[&ACTIVE], grace).await;
    STOP.cancel();
    // A connection accepted right at STOP is counted once its loop has handed it over. Ones
    // from before the drain still open now (streams) are cut at the exec, so only late ones wait.
    settle(&[&ACCEPTING], Duration::from_secs(1)).await;
    settle(&[&LATE], LATE_GRACE).await;
    let late = LATE.load(Ordering::SeqCst);
    if late > 0 {
        tracing::info!("{late} connections accepted during the drain are still open and close at the exec");
    }
}

async fn settle(counts: &[&AtomicUsize], limit: Duration) {
    let started = Instant::now();
    while counts.iter().any(|c| c.load(Ordering::SeqCst) > 0) && started.elapsed() < limit {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    // The only test that touches the process-wide handoff state.
    #[tokio::test]
    async fn keeps_accepting_while_old_connections_drain() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let accepting = tokio::spawn(accept(listener, move |s, _| drop(tx.send(s))));
        // A stream from before the drain that outlives the grace, like a panel tab's events.
        let old = ConnGuard::new();
        let draining_task = tokio::spawn(drain(Duration::from_secs(1)));
        while !draining() {
            tokio::task::yield_now().await;
        }
        timeout(Duration::from_millis(10), old.closing()).await.expect("an old connection closes when the drain starts");
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        timeout(Duration::from_millis(500), rx.recv()).await.expect("a visitor during the drain is accepted").unwrap();
        let late = ConnGuard::new();
        assert!(timeout(Duration::from_millis(10), late.closing()).await.is_err(), "a late connection keeps serving");
        timeout(Duration::from_secs(2), accepting).await.expect("accepting stops when the grace runs out").unwrap();
        timeout(Duration::from_millis(10), late.closing()).await.expect("late connections close before the exec");
        assert!(!draining_task.is_finished(), "the exec waits for late connections");
        drop(late);
        timeout(Duration::from_secs(1), draining_task).await.expect("an old stream still open doesn't hold the exec").unwrap();
        drop(old);
    }
}
