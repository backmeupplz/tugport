//! Scale to zero: services with `sleep` set stop when idle and wake on the next request.

use crate::app::App;

pub fn is_asleep(app: &App, service_id: &str) -> bool {
    app.db.get_state(&format!("asleep:{service_id}")).ok().flatten().is_some()
}
