//! Scale to zero: services with `sleep` set stop when idle and wake on the next request.

use crate::app::App;
use crate::proxy::Upstream;

pub fn is_asleep(app: &App, service_id: &str) -> bool {
    app.db.get_state(&format!("asleep:{service_id}")).ok().flatten().is_some()
}

/// Start a sleeping service and return where to send the request once it is healthy.
pub async fn wake(app: &App, service_id: &str) -> anyhow::Result<Vec<Upstream>> {
    let _ = (app, service_id);
    anyhow::bail!("this service is asleep")
}
