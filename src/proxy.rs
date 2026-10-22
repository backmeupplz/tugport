//! The built-in reverse proxy: HTTP(S) routing, certificates, middlewares.

use crate::app::App;

/// Rebuild the routing table from the database and running containers.
pub fn reload(app: &App) {
    let _ = app;
}
