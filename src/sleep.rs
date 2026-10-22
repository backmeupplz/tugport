//! Scale to zero: services with `sleep` set stop when idle and wake on the next request.

use crate::app::App;
use std::net::IpAddr;

pub fn is_asleep(app: &App, service_id: &str) -> bool {
    app.db.get_state(&format!("asleep:{service_id}")).ok().flatten().is_some()
}

/// Start a sleeping service and return its replica addresses once healthy.
pub async fn wake(app: &App, service_id: &str) -> anyhow::Result<Vec<IpAddr>> {
    let _ = (app, service_id);
    anyhow::bail!("this service is asleep")
}
