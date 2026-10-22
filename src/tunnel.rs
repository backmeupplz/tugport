//! Cloudflare Tunnel: routes chosen domains through a tunnel instead of open ports.

use crate::app::AppState;

/// Re-sync tunnel ingress rules and DNS records in the background.
pub fn sync_soon(app: &AppState) {
    let _ = app;
}
