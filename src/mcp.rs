//! MCP endpoint (streamable HTTP, JSON responses). Every tool is a thin wrapper that
//! replays a REST call with the caller's own credentials, so permissions are identical.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use serde_json::{Map, Value, json};
use std::sync::OnceLock;
use tower::ServiceExt;

static ROUTER: OnceLock<Router> = OnceLock::new();

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

fn tools() -> Vec<Tool> {
    let service = s("Service id or `project.service`, e.g. `shop.api`");
    vec![
        Tool { name: "list_projects", description: "List projects you can see, with your role in each.", method: Method::GET, path: "/api/projects", query: &[], schema: obj(json!({}), &[]) },
        Tool { name: "get_project", description: "A project's services with live status, and its shared variable names.", method: Method::GET, path: "/api/projects/{project}", query: &[], schema: obj(json!({ "project": s("Project id or name") }), &["project"]) },
        Tool { name: "create_project", description: "Create a project.", method: Method::POST, path: "/api/projects", query: &[], schema: obj(json!({ "name": s("Lowercase DNS-safe name"), "description": s("Optional") }), &["name"]) },
        Tool {
            name: "create_service",
            description: "Create a service in a project. kind is app, database or compose. For apps pass spec.source ({type: git, repo, branch} | {type: image, image} | {type: dockerfile, content}); for databases pass database: {engine: postgres|mysql|mariadb|mongo|redis}.",
            method: Method::POST,
            path: "/api/projects/{project}/services",
            query: &[],
            schema: obj(json!({ "project": s("Project id or name"), "name": s("Service name"), "kind": s("app, database or compose"),
                                "spec": { "type": "object", "description": "Service spec (see docs/API.md)" },
                                "database": { "type": "object", "description": "{engine, user?, database?}" },
                                "env": { "type": "object", "description": "Initial env vars {NAME: value}" } }), &["project", "name"]),
        },
        Tool { name: "get_service", description: "A service's spec, live status, replicas, domains, env var names and last deploy.", method: Method::GET, path: "/api/services/{service}", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool { name: "update_service", description: "Change a service's settings with a JSON merge patch of its spec. Redeploy to apply.", method: Method::PATCH, path: "/api/services/{service}", query: &[], schema: obj(json!({ "service": service, "spec": { "type": "object" } }), &["service", "spec"]) },
        Tool { name: "delete_service", description: "Delete a service (needs the destroy grant). volumes=true also deletes its data.", method: Method::DELETE, path: "/api/services/{service}", query: &["volumes"], schema: obj(json!({ "service": service, "volumes": { "type": "boolean" } }), &["service"]) },
        Tool { name: "deploy", description: "Deploy a service and wait for the result (set wait=false to return immediately). Returns the deploy and the end of its log.", method: Method::POST, path: "/api/services/{service}/deploy", query: &[], schema: obj(json!({ "service": service, "ref": s("Branch, tag or commit (Git services)"), "wait": { "type": "boolean", "default": true } }), &["service"]) },
        Tool { name: "list_deploys", description: "Recent deploys of a service.", method: Method::GET, path: "/api/services/{service}/deploys", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool { name: "get_deploy", description: "A deploy with its full log.", method: Method::GET, path: "/api/deploys/{deploy_id}", query: &[], schema: obj(json!({ "deploy_id": s("Deploy id") }), &["deploy_id"]) },
        Tool { name: "cancel_deploy", description: "Cancel a running or queued deploy.", method: Method::POST, path: "/api/deploys/{deploy_id}/cancel", query: &[], schema: obj(json!({ "deploy_id": s("Deploy id") }), &["deploy_id"]) },
        Tool { name: "rollback", description: "Roll back to the previous version (or a given deploy_id).", method: Method::POST, path: "/api/services/{service}/rollback", query: &[], schema: obj(json!({ "service": service, "deploy_id": s("Optional deploy to roll back to") }), &["service"]) },
        Tool { name: "restart", description: "Restart a service's containers.", method: Method::POST, path: "/api/services/{service}/restart", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool { name: "stop", description: "Stop a service.", method: Method::POST, path: "/api/services/{service}/stop", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool { name: "start", description: "Start a stopped service.", method: Method::POST, path: "/api/services/{service}/start", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool {
            name: "get_logs",
            description: "Search a service's logs. q is a case-insensitive substring; since/until are epoch milliseconds.",
            method: Method::GET,
            path: "/api/services/{service}/logs",
            query: &["q", "since", "until", "limit", "replica"],
            schema: obj(json!({ "service": service, "q": s("Text to find"), "since": { "type": "integer" }, "until": { "type": "integer" }, "limit": { "type": "integer", "default": 200 }, "replica": s("Container name") }), &["service"]),
        },
        Tool { name: "exec", description: "Run a shell command inside a service's running container (with its env) and return the output.", method: Method::POST, path: "/api/services/{service}/exec", query: &[], schema: obj(json!({ "service": service, "command": s("Shell command"), "timeout_s": { "type": "integer", "default": 60 } }), &["service", "command"]) },
        Tool { name: "list_env", description: "A service's env var names (values only if you hold secrets:read).", method: Method::GET, path: "/api/services/{service}/env", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool { name: "set_env", description: "Set or delete env vars: vars is {NAME: \"value\" or null}. Redeploy to apply.", method: Method::PUT, path: "/api/services/{service}/env", query: &[], schema: obj(json!({ "service": service, "vars": { "type": "object" } }), &["service", "vars"]) },
        Tool { name: "add_domain", description: "Serve a service on a domain with automatic HTTPS.", method: Method::POST, path: "/api/services/{service}/domains", query: &[], schema: obj(json!({ "service": service, "host": s("example.com"), "port": { "type": "integer", "description": "Container port" }, "path": s("Path prefix, default /"), "https": { "type": "boolean" }, "via_tunnel": { "type": "boolean" } }), &["service", "host", "port"]) },
        Tool { name: "remove_domain", description: "Remove a domain by id.", method: Method::DELETE, path: "/api/domains/{domain_id}", query: &[], schema: obj(json!({ "domain_id": s("Domain id") }), &["domain_id"]) },
        Tool { name: "list_services", description: "Every service you can see across projects, with its state.", method: Method::GET, path: "/api/services", query: &[], schema: obj(json!({}), &[]) },
        Tool { name: "list_servers", description: "The servers in this setup (the panel's own is id local) with status, versions and size.", method: Method::GET, path: "/api/nodes", query: &[], schema: obj(json!({}), &[]) },
        Tool { name: "server_status", description: "A server's current CPU, memory, disks, network, GPUs and per-service usage (default: the panel's own).", method: Method::GET, path: "/api/metrics/now", query: &["node"], schema: obj(json!({ "node": s("Server id from list_servers") }), &[]) },
        Tool {
            name: "metrics",
            description: "Time series for a server (scope=host), a GPU (gpu:0) or a service (service:<id>, added up over every server). range is 1h, 6h, 24h, 7d or 30d.",
            method: Method::GET,
            path: "/api/metrics",
            query: &["scope", "names", "range", "node"],
            schema: obj(json!({ "scope": s("host, gpu:<n> or service:<id>"), "names": s("Comma-separated series, e.g. cpu,memory,restarts"), "range": s("1h"), "node": s("Server id for host and GPU scopes") }), &[]),
        },
        Tool { name: "list_replicas", description: "A database's read replicas and whether they follow it.", method: Method::GET, path: "/api/services/{service}/replicas", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool {
            name: "add_replica",
            description: "Add a read replica to a database, on a given server or region (placement {node, region, labels}). MongoDB and MariaDB primaries restart once the first time.",
            method: Method::POST,
            path: "/api/services/{service}/replicas",
            query: &[],
            schema: obj(json!({ "service": service, "placement": { "type": "object" } }), &["service"]),
        },
        Tool { name: "promote_replica", description: "Make a replica a primary when its primary is lost (admin). The old primary's URL variable then points at it.", method: Method::POST, path: "/api/services/{service}/promote", query: &[], schema: obj(json!({ "service": service }), &["service"]) },
        Tool { name: "list_backups", description: "Backup jobs with their last run.", method: Method::GET, path: "/api/backups/jobs", query: &[], schema: obj(json!({}), &[]) },
        Tool { name: "run_backup", description: "Run a backup job now.", method: Method::POST, path: "/api/backups/jobs/{job_id}/run", query: &[], schema: obj(json!({ "job_id": s("Backup job id") }), &["job_id"]) },
        Tool { name: "audit_log", description: "Who did what, newest first.", method: Method::GET, path: "/api/audit", query: &["project", "service", "actor", "limit"], schema: obj(json!({ "project": s("Project id"), "service": s("Service id"), "actor": s("User or agent name"), "limit": { "type": "integer" } }), &[]) },
    ]
}

const PROTOCOL: &str = "2025-06-18";

pub async fn handle(req: Request) -> Response {
    let conn = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
    let headers = req.headers().clone();
    let Ok(body) = req.into_body().collect().await.map(|b| b.to_bytes()) else {
        return (StatusCode::BAD_REQUEST, axum::Json(rpc_error(Value::Null, -32700, "unreadable body"))).into_response();
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
    let id = m.get("id").cloned();
    let method = m["method"].as_str().unwrap_or_default();
    let id = id?; // notifications get no response
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
        "tools/list" => json!({ "tools": tools().iter().map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.schema })).collect::<Vec<_>>() }),
        "tools/call" => {
            let name = m["params"]["name"].as_str().unwrap_or_default();
            let args = m["params"]["arguments"].as_object().cloned().unwrap_or_default();
            match tools().into_iter().find(|t| t.name == name) {
                Some(t) => call(&t, args, headers, conn).await,
                None => return Some(rpc_error(id, -32602, &format!("unknown tool {name}"))),
            }
        }
        other => return Some(rpc_error(id, -32601, &format!("method {other} not found"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn call(t: &Tool, mut args: Map<String, Value>, headers: &HeaderMap, conn: Option<std::net::SocketAddr>) -> Value {
    let Some(router) = ROUTER.get() else { return text_result("the server is starting", true) };
    let mut path = t.path.to_string();
    while let Some(start) = path.find('{') {
        let Some(end) = path[start..].find('}').map(|e| start + e) else { break };
        let key = path[start + 1..end].to_string();
        let Some(v) = args.remove(&key) else { return text_result(&format!("missing argument {key}"), true) };
        let v = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
        path.replace_range(start..=end, &urlencode(&v));
    }
    let query: Vec<String> = t
        .query
        .iter()
        .filter_map(|k| args.remove(*k).map(|v| format!("{k}={}", urlencode(&v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))))
        .collect();
    if !query.is_empty() {
        path = format!("{path}?{}", query.join("&"));
    }
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
    for name in [header::AUTHORIZATION, header::COOKIE, header::HOST] {
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
            let text = String::from_utf8_lossy(&bytes).to_string();
            let structured: Option<Value> = serde_json::from_str(&text).ok();
            let mut r = text_result(&text, !status.is_success());
            if let Some(Value::Object(o)) = structured {
                r["structuredContent"] = Value::Object(o);
            }
            r
        }
        Err(e) => text_result(&format!("request failed: {e}"), true),
    }
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn urlencode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
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
}
