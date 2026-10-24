use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist"]
struct Assets;

pub async fn static_handler(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": "no such endpoint" }))).into_response();
    }
    // Anything else is a panel route, which the app's index.html renders.
    let path = if Assets::get(path).is_some() { path } else { "index.html" };
    // The build writes .br and .gz copies next to each file (web/compress.mjs); over a slow
    // link the bundle arrives about three times faster.
    let accepts = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("");
    let encoded = [("br", "br"), ("gzip", "gz")]
        .into_iter()
        .filter(|(name, _)| accepts.split(',').any(|e| e.split(';').next().is_some_and(|e| e.trim() == *name)))
        .find_map(|(name, ext)| Some((name, Assets::get(&format!("{path}.{ext}"))?)));
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let base = [(header::CONTENT_TYPE, mime(path)), (header::CACHE_CONTROL, cache), (header::VARY, "accept-encoding")];
    match encoded {
        Some((name, file)) => (base, [(header::CONTENT_ENCODING, name)], file.data).into_response(),
        None => match Assets::get(path) {
            Some(file) => (base, file.data).into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    }
}

/// Content types of what the panel's build emits.
fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("json" | "map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
