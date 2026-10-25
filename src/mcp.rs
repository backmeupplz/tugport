//! MCP endpoint (streamable HTTP, JSON responses). Every tool is a thin wrapper that
//! replays a REST call with the caller's own credentials, so permissions are identical.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use std::sync::{LazyLock, OnceLock};
use tower::ServiceExt;

static ROUTER: OnceLock<Router> = OnceLock::new();
static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(tools);

pub fn install(router: Router) {
    let _ = ROUTER.set(router);
}

struct Tool {
    name: &'static str,
    description: &'static str,
    method: Method,
    path: &'static str,
    /// Arguments sent as query parameters (the rest of the non-path arguments form the body).
    query: &'static [&'static str],
    schema: Value,
}

fn s(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn tool(name: &'static str, method: Method, path: &'static str, description: &'static str, schema: Value) -> Tool {
    Tool { name, description, method, path, query: &[], schema }
}

fn tools() -> Vec<Tool> {
    use Method as M;
    let service = s("Service id or `project.service`, e.g. `shop.api`");
    let on_service = || obj(json!({ "service": service }), &["service"]);
    let on_deploy = || obj(json!({ "deploy_id": s("Deploy id") }), &["deploy_id"]);
    let none = || obj(json!({}), &[]);
    vec![
        tool("list_projects", M::GET, "/api/projects", "List projects you can see, with your role in each.", none()),
        tool("get_project", M::GET, "/api/projects/{project}", "A project's services with live status, and its shared variable names.", obj(json!({ "project": s("Project id or name") }), &["project"])),
        tool("create_project", M::POST, "/api/projects", "Create a project.", obj(json!({ "name": s("Lowercase DNS-safe name"), "description": s("Optional") }), &["name"])),
        tool(
            "create_service",
            M::POST,
            "/api/projects/{project}/services",
            "Create a service in a project. kind is app, database or compose. For apps pass spec.source ({type: git, repo, branch} | {type: image, image} | {type: dockerfile, content}); for databases pass database: {engine: postgres|mysql|mariadb|mongo|redis}.",
            obj(json!({ "project": s("Project id or name"), "name": s("Service name"), "kind": s("app, database or compose"),
                        "spec": { "type": "object", "description": "Service spec (see docs/API.md)" },
                        "database": { "type": "object", "description": "{engine, user?, database?}" },
                        "env": { "type": "object", "description": "Initial env vars {NAME: value}" } }), &["project", "name"]),
        ),
        tool("get_service", M::GET, "/api/services/{service}", "A service's spec, live status, replicas, domains, env var names and last deploy.", on_service()),
        tool("update_service", M::PATCH, "/api/services/{service}", "Change a service's settings with a JSON merge patch of its spec. Redeploy to apply.", obj(json!({ "service": service, "spec": { "type": "object" } }), &["service", "spec"])),
        Tool { query: &["volumes"], ..tool("delete_service", M::DELETE, "/api/services/{service}", "Delete a service (needs the destroy grant). volumes=true also deletes its data.", obj(json!({ "service": service, "volumes": { "type": "boolean" } }), &["service"])) },
        tool("deploy", M::POST, "/api/services/{service}/deploy", "Deploy a service and wait for the result (set wait=false to return immediately). Returns the deploy and the end of its log.", obj(json!({ "service": service, "ref": s("Branch, tag or commit (Git services)"), "no_cache": { "type": "boolean", "default": false, "description": "Build without the layer cache and pull base images again" }, "wait": { "type": "boolean", "default": true } }), &["service"])),
        tool("list_deploys", M::GET, "/api/services/{service}/deploys", "Recent deploys of a service.", on_service()),
        tool("get_deploy", M::GET, "/api/deploys/{deploy_id}", "A deploy with its full log.", on_deploy()),
        tool("cancel_deploy", M::POST, "/api/deploys/{deploy_id}/cancel", "Cancel a running or queued deploy.", on_deploy()),
        tool("rollback", M::POST, "/api/services/{service}/rollback", "Roll back to the previous version (or a given deploy_id).", obj(json!({ "service": service, "deploy_id": s("Optional deploy to roll back to") }), &["service"])),
        tool("restart", M::POST, "/api/services/{service}/restart", "Restart a service's containers.", on_service()),
        tool("stop", M::POST, "/api/services/{service}/stop", "Stop a service.", on_service()),
        tool("start", M::POST, "/api/services/{service}/start", "Start a stopped service.", on_service()),
        Tool {
            query: &["q", "since", "until", "limit", "replica"],
            ..tool("get_logs", M::GET, "/api/services/{service}/logs", "Search a service's logs. q is a case-insensitive substring; since/until are epoch milliseconds.",
                obj(json!({ "service": service, "q": s("Text to find"), "since": { "type": "integer" }, "until": { "type": "integer" }, "limit": { "type": "integer", "default": 200 }, "replica": s("Container name") }), &["service"]))
        },
        tool("exec", M::POST, "/api/services/{service}/exec", "Run a shell command inside a service's running container (with its env) and return the output.", obj(json!({ "service": service, "command": s("Shell command"), "timeout_s": { "type": "integer", "default": 60 } }), &["service", "command"])),
        tool("list_env", M::GET, "/api/services/{service}/env", "A service's env var names (values only if you hold secrets:read).", on_service()),
        tool("set_env", M::PUT, "/api/services/{service}/env", "Set or delete env vars: vars is {NAME: \"value\" or null}. Redeploy to apply.", obj(json!({ "service": service, "vars": { "type": "object" } }), &["service", "vars"])),
        tool("add_domain", M::POST, "/api/services/{service}/domains", "Serve a service on a domain with automatic HTTPS.", obj(json!({ "service": service, "host": s("example.com"), "port": { "type": "integer", "description": "Container port" }, "path": s("Path prefix, default /"), "https": { "type": "boolean" }, "via_tunnel": { "type": "boolean" } }), &["service", "host", "port"])),
        tool("remove_domain", M::DELETE, "/api/domains/{domain_id}", "Remove a domain by id.", obj(json!({ "domain_id": s("Domain id") }), &["domain_id"])),
        tool("list_services", M::GET, "/api/services", "Every service you can see across projects, with its state.", none()),
        tool("list_servers", M::GET, "/api/nodes", "The servers in this setup (the panel's own is id local) with status, versions and size.", none()),
        Tool { query: &["node"], ..tool("server_status", M::GET, "/api/metrics/now", "A server's current CPU, memory, disks, network, GPUs and per-service usage (default: the panel's own).", obj(json!({ "node": s("Server id from list_servers") }), &[])) },
        Tool {
            query: &["scope", "names", "range", "node"],
            ..tool("metrics", M::GET, "/api/metrics", "Time series for a server (scope=host), a GPU (gpu:0) or a service (service:<id>, added up over every server). range is 1h, 6h, 24h, 7d or 30d.",
                obj(json!({ "scope": s("host, gpu:<n> or service:<id>"), "names": s("Comma-separated series, e.g. cpu,memory,restarts"), "range": s("1h"), "node": s("Server id for host and GPU scopes") }), &[]))
        },
        tool("list_replicas", M::GET, "/api/services/{service}/replicas", "A database's read replicas and whether they follow it.", on_service()),
        tool("add_replica", M::POST, "/api/services/{service}/replicas", "Add a read replica to a database, on a given server or region (placement {node, region, labels}). MongoDB and MariaDB primaries restart once the first time.", obj(json!({ "service": service, "placement": { "type": "object" } }), &["service"])),
        tool("promote_replica", M::POST, "/api/services/{service}/promote", "Make a replica a primary when its primary is lost (admin). The old primary's URL variable then points at it.", on_service()),
        tool("list_backups", M::GET, "/api/backups/jobs", "Backup jobs with their last run.", none()),
        tool("run_backup", M::POST, "/api/backups/jobs/{job_id}/run", "Run a backup job now.", obj(json!({ "job_id": s("Backup job id") }), &["job_id"])),
        Tool { query: &["project", "service", "actor", "limit"], ..tool("audit_log", M::GET, "/api/audit", "Who did what, newest first.", obj(json!({ "project": s("Project id"), "service": s("Service id"), "actor": s("User or agent name"), "limit": { "type": "integer" } }), &[])) },
    ]
}

const PROTOCOL: &str = "2025-06-18";

pub async fn handle(req: Request) -> Response {
    let conn = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    let headers = req.headers().clone();
    // Read before anything checks the caller, so bounded like other JSON bodies.
    let Ok(body) = axum::body::to_bytes(req.into_body(), 2 << 20).await else {
        return (StatusCode::BAD_REQUEST, axum::Json(rpc_error(Value::Null, -32700, "unreadable or too large body"))).into_response();
    };
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, axum::Json(rpc_error(Value::Null, -32700, "parse error"))).into_response();
    };
    match msg {
        Value::Array(batch) => {
            let mut out = vec![];
            for m in batch {
                if let Some(r) = one(&headers, conn, m).await {
                    out.push(r);
                }
            }
            if out.is_empty() { StatusCode::ACCEPTED.into_response() } else { axum::Json(Value::Array(out)).into_response() }
        }
        m => match one(&headers, conn, m).await {
            Some(r) => axum::Json(r).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn one(headers: &HeaderMap, conn: Option<std::net::SocketAddr>, m: Value) -> Option<Value> {
    let id = m.get("id").cloned()?; // notifications get no response
    let method = m["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => {
            let asked = m["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL);
            json!({
                "protocolVersion": if ["2025-06-18", "2025-03-26", "2024-11-05", "2025-11-25"].contains(&asked) { asked } else { PROTOCOL },
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "tugport", "version": crate::VERSION },
                "instructions": "Tugport deploys and runs services on this server. Address services as `project.service`. Deploys wait for the result by default.",
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": TOOLS.iter().map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.schema })).collect::<Vec<_>>() }),
        "tools/call" => {
            let name = m["params"]["name"].as_str().unwrap_or_default();
            let args = m["params"]["arguments"].as_object().cloned().unwrap_or_default();
            match TOOLS.iter().find(|t| t.name == name) {
                Some(t) => call(t, args, headers, conn).await,
                None => return Some(rpc_error(id, -32602, &format!("unknown tool {name}"))),
            }
        }
        other => return Some(rpc_error(id, -32601, &format!("method {other} not found"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn call(t: &Tool, mut args: Map<String, Value>, headers: &HeaderMap, conn: Option<std::net::SocketAddr>) -> Value {
    let Some(router) = ROUTER.get() else { return text_result("the server is starting", true) };
    let path = match uri(t, &mut args) {
        Ok(p) => p,
        Err(key) => return text_result(&format!("missing argument {key}"), true),
    };
    // Tool-specific argument shapes.
    let body = match t.name {
        "set_env" => args.remove("vars").unwrap_or(json!({})),
        "deploy" => {
            args.entry("wait").or_insert(Value::Bool(true));
            Value::Object(args)
        }
        _ => Value::Object(args),
    };
    let mut req = Request::builder().method(t.method.clone()).uri(&path).header(header::CONTENT_TYPE, "application/json");
    // MCP clients authenticate with an API key; a browser's session cookie is not forwarded.
    for name in [header::AUTHORIZATION, header::HOST] {
        if let Some(v) = headers.get(&name) {
            req = req.header(name, v);
        }
    }
    let payload = if t.method == Method::GET || t.method == Method::DELETE { Body::empty() } else { Body::from(body.to_string()) };
    let Ok(mut req) = req.body(payload) else { return text_result("could not build the request", true) };
    if let Some(c) = conn {
        req.extensions_mut().insert(axum::extract::ConnectInfo(c));
    }
    match router.clone().oneshot(req).await {
        Ok(resp) => {
            let status = resp.status();
            let bytes = resp.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            let mut r = text_result(&text, !status.is_success());
            if let Ok(Value::Object(o)) = serde_json::from_str(&text) {
                r["structuredContent"] = Value::Object(o);
            }
            r
        }
        Err(e) => text_result(&format!("request failed: {e}"), true),
    }
}

/// The tool's path with its `{placeholders}` and query arguments filled in (taken out of
/// `args`), or the name of a missing path argument.
fn uri<'a>(t: &'a Tool, args: &mut Map<String, Value>) -> Result<String, &'a str> {
    // A string argument as itself, anything else as its JSON.
    let encode = |v: Value| match v.as_str() {
        Some(s) => crate::util::urlencode(s),
        None => crate::util::urlencode(&v.to_string()),
    };
    let mut path = String::new();
    let mut rest = t.path;
    while let Some((before, after)) = rest.split_once('{')
        && let Some((key, after)) = after.split_once('}')
    {
        path.push_str(before);
        path.push_str(&encode(args.remove(key).ok_or(key)?));
        rest = after;
    }
    path.push_str(rest);
    let query: Vec<String> = t.query.iter().filter_map(|k| args.remove(*k).map(|v| format!("{k}={}", encode(v)))).collect();
    if !query.is_empty() {
        path = format!("{path}?{}", query.join("&"));
    }
    Ok(path)
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_valid_names_and_paths() {
        let ts = tools();
        let mut names: Vec<&str> = ts.iter().map(|t| t.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ts.len(), "tool names are unique");
        for t in &ts {
            for part in t.path.split('{').skip(1) {
                let key = part.split('}').next().unwrap();
                assert!(t.schema["properties"].get(key).is_some(), "{} declares {key}", t.name);
            }
        }
    }

    /// Every tool replays a route the REST router has, with the same method.
    #[test]
    fn tools_match_the_router() {
        // `/api/services/{id}` and `/api/services/{service}` are the same route.
        let norm = |path: &str| path.split('{').map(|p| p.split_once('}').map_or(p, |(_, rest)| rest)).collect::<Vec<_>>().join("{}");
        let routes: Vec<(String, &str)> = include_str!("api/mod.rs")
            .lines()
            .filter_map(|l| l.trim().strip_prefix(".route(\"")?.split_once("\", "))
            .map(|(path, handlers)| (norm(path), handlers))
            .collect();
        for t in tools() {
            let path = norm(t.path);
            let method = format!("{}(", t.method.as_str().to_ascii_lowercase());
            assert!(routes.iter().any(|(p, h)| *p == path && (h.starts_with(&method) || h.contains(&format!(".{method}")))), "{} {} {}", t.name, t.method, t.path);
        }
    }

    #[test]
    fn fills_paths_and_queries() {
        let ts = tools();
        let find = |n: &str| ts.iter().find(|t| t.name == n).unwrap();
        let mut args = json!({ "service": "shop.api", "q": "a b", "limit": 5, "other": 1 }).as_object().cloned().unwrap();
        assert_eq!(uri(find("get_logs"), &mut args).unwrap(), "/api/services/shop.api/logs?q=a%20b&limit=5");
        assert_eq!(args.keys().collect::<Vec<_>>(), ["other"]);
        assert_eq!(uri(find("get_deploy"), &mut Map::new()), Err("deploy_id"));
        assert_eq!(uri(find("list_projects"), &mut Map::new()).unwrap(), "/api/projects");
    }
}
