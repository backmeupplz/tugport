//! Zero-downtime restarts. Listening sockets are kept open across a re-exec of the new
//! binary (same PID, so systemd sees nothing); while the new process starts, the kernel
//! queues incoming connections instead of refusing them.

use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const ENV: &str = "TUGPORT_LISTEN_FDS";

static DRAINING: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static FDS: Mutex<Vec<(String, RawFd)>> = Mutex::new(Vec::new());
/// Woken when the process starts draining for a handoff.
pub static DRAIN: tokio::sync::Notify = tokio::sync::Notify::const_new();

pub fn draining() -> bool {
    DRAINING.load(Ordering::SeqCst)
}

/// Counts a connection as in flight while alive.
pub struct ConnGuard;

impl ConnGuard {
    pub fn new() -> Self {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        ConnGuard
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn inherited(key: &str) -> Option<RawFd> {
    std::env::var(ENV).ok()?.split(',').find_map(|pair| {
        let (k, fd) = pair.split_once('=')?;
        (k == key).then(|| fd.parse().ok()).flatten()
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
            std_listener.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(std_listener).with_context(|| format!("adopting the inherited {key} socket"))?
        }
        None => bind.await?,
    };
    // A duplicate (without close-on-exec) keeps the socket open for the next handoff.
    // SAFETY: dup on a valid descriptor we own; the result is only stored, never closed twice.
    let dup = unsafe { libc::dup(l.as_raw_fd()) };
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

/// Stop accepting, let in-flight connections finish (up to `grace`), then exec `binary`
/// with the same arguments, passing the listening sockets along. The binary is tried
/// first, so a broken one leaves this process serving.
pub async fn reexec(binary: &std::path::Path, grace: Duration) -> Result<()> {
    let probe = tokio::time::timeout(Duration::from_secs(10), tokio::process::Command::new(binary).arg("version").output()).await;
    match probe {
        Ok(Ok(out)) if out.status.success() => {}
        _ => bail!("{} does not run; staying on the current version", binary.display()),
    }
    DRAINING.store(true, Ordering::SeqCst);
    DRAIN.notify_waiters();
    let started = Instant::now();
    while ACTIVE.load(Ordering::SeqCst) > 0 && started.elapsed() < grace {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fds = crate::util::lock(&FDS).iter().map(|(k, fd)| format!("{k}={fd}")).collect::<Vec<_>>().join(",");
    tracing::info!("handing {} sockets to {}", fds.matches('=').count(), binary.display());
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(binary).args(std::env::args_os().skip(1)).env(ENV, fds).exec();
    // exec only returns on failure. Listeners have stopped, so exit and let the
    // service manager start the process again rather than linger without serving.
    tracing::error!("could not start {}: {err}; exiting so the service restarts", binary.display());
    std::process::exit(1)
}
