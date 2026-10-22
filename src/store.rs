//! Reading and writing projects, services, env vars, files and domains.

use crate::app::App;
use crate::model::{DomainOptions, ServiceSpec};
use crate::util;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use std::collections::BTreeMap;
use zeroize::Zeroizing;

#[derive(Serialize, Clone, Debug)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Service {
    pub id: String,
    pub project_id: String,
    pub project: String,
    pub name: String,
    pub kind: String,
    pub spec: ServiceSpec,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Service {
    /// Prefix of container and image names: `tp-shop-api`.
    pub fn slug(&self) -> String {
        format!("tp-{}-{}", self.project, self.name)
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Domain {
    pub id: String,
    pub service_id: String,
    pub host: String,
    pub path: String,
    pub port: u16,
    pub container: Option<String>,
    pub https: bool,
    pub via_tunnel: bool,
    #[serde(serialize_with = "crate::model::public_options")]
    pub options: DomainOptions,
}

pub fn project(app: &App, id_or_name: &str) -> Result<Option<Project>> {
    Ok(app
        .db
        .conn()
        .query_row(
            "SELECT id, name, description, created_at FROM projects WHERE id = ?1 OR name = ?1",
            [id_or_name],
            |r| Ok(Project { id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, created_at: r.get(3)? }),
        )
        .optional()?)
}

pub fn projects(app: &App) -> Result<Vec<Project>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT id, name, description, created_at FROM projects ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| Ok(Project { id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, created_at: r.get(3)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

const SERVICE_SELECT: &str = "SELECT s.id, s.project_id, p.name, s.name, s.kind, s.spec, s.created_at, s.updated_at FROM services s JOIN projects p ON p.id = s.project_id";

fn service_row(r: &rusqlite::Row) -> rusqlite::Result<(Service, String)> {
    Ok((
        Service {
            id: r.get(0)?,
            project_id: r.get(1)?,
            project: r.get(2)?,
            name: r.get(3)?,
            kind: r.get(4)?,
            spec: ServiceSpec::default(),
            created_at: r.get(6)?,
            updated_at: r.get(7)?,
        },
        r.get(5)?,
    ))
}

fn with_spec((mut s, raw): (Service, String)) -> Result<Service> {
    s.spec = serde_json::from_str(&raw).with_context(|| format!("service {} has an unreadable spec", s.id))?;
    Ok(s)
}

/// A service by id or `project.service`.
pub fn service(app: &App, reference: &str) -> Result<Option<Service>> {
    let row = if let Some((p, s)) = reference.split_once('.') {
        app.db
            .conn()
            .query_row(&format!("{SERVICE_SELECT} WHERE p.name = ?1 AND s.name = ?2"), params![p, s], service_row)
            .optional()?
    } else {
        app.db.conn().query_row(&format!("{SERVICE_SELECT} WHERE s.id = ?1"), [reference], service_row).optional()?
    };
    row.map(with_spec).transpose()
}

pub fn services(app: &App, project_id: Option<&str>) -> Result<Vec<Service>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(&format!("{SERVICE_SELECT} WHERE ?1 IS NULL OR s.project_id = ?1 ORDER BY p.name, s.name"))?;
    let rows = stmt.query_map([project_id], service_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    drop(conn);
    rows.into_iter().map(with_spec).collect()
}

pub fn save_spec(app: &App, service_id: &str, spec: &ServiceSpec) -> Result<()> {
    app.db.conn().execute(
        "UPDATE services SET spec = ?1, updated_at = ?2 WHERE id = ?3",
        params![serde_json::to_string(spec)?, util::now(), service_id],
    )?;
    Ok(())
}

fn env_context(service_id: &str, key: &str) -> String {
    format!("env:{service_id}:{key}")
}

fn var_context(project_id: &str, key: &str) -> String {
    format!("var:{project_id}:{key}")
}

/// Every variable a service's containers get: project variables, overridden by the service's own.
pub fn resolved_env(app: &App, service: &Service) -> Result<BTreeMap<String, Zeroizing<String>>> {
    let mut out = BTreeMap::new();
    for (k, v) in project_vars(app, &service.project_id)? {
        out.insert(k, v);
    }
    for (k, v) in service_env(app, &service.id)? {
        out.insert(k, v);
    }
    Ok(out)
}

pub fn service_env(app: &App, service_id: &str) -> Result<BTreeMap<String, Zeroizing<String>>> {
    let rows: Vec<(String, Vec<u8>)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT key, value FROM service_env WHERE service_id = ?1")?;
        stmt.query_map([service_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    rows.into_iter().map(|(k, v)| Ok((k.clone(), app.keys.open_str(&env_context(service_id, &k), &v)?))).collect()
}

pub fn env_names(app: &App, service_id: &str) -> Result<Vec<String>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare("SELECT key FROM service_env WHERE service_id = ?1 ORDER BY key")?;
    Ok(stmt.query_map([service_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

pub fn set_env(app: &App, service_id: &str, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) => app.db.conn().execute(
            "INSERT INTO service_env (service_id, key, value) VALUES (?1, ?2, ?3) ON CONFLICT(service_id, key) DO UPDATE SET value = excluded.value",
            params![service_id, key, app.keys.seal_str(&env_context(service_id, key), v)?],
        )?,
        None => app.db.conn().execute("DELETE FROM service_env WHERE service_id = ?1 AND key = ?2", params![service_id, key])?,
    };
    Ok(())
}

pub fn project_vars(app: &App, project_id: &str) -> Result<BTreeMap<String, Zeroizing<String>>> {
    let rows: Vec<(String, Vec<u8>)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT key, value FROM project_vars WHERE project_id = ?1")?;
        stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    rows.into_iter().map(|(k, v)| Ok((k.clone(), app.keys.open_str(&var_context(project_id, &k), &v)?))).collect()
}

pub fn set_project_var(app: &App, project_id: &str, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) => app.db.conn().execute(
            "INSERT INTO project_vars (project_id, key, value) VALUES (?1, ?2, ?3) ON CONFLICT(project_id, key) DO UPDATE SET value = excluded.value",
            params![project_id, key, app.keys.seal_str(&var_context(project_id, key), v)?],
        )?,
        None => app.db.conn().execute("DELETE FROM project_vars WHERE project_id = ?1 AND key = ?2", params![project_id, key])?,
    };
    Ok(())
}

pub fn valid_env_key(key: &str) -> bool {
    let b = key.as_bytes();
    !b.is_empty() && !b[0].is_ascii_digit() && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

pub fn files(app: &App, service_id: &str) -> Result<Vec<(String, Zeroizing<String>)>> {
    let rows: Vec<(String, Vec<u8>)> = {
        let conn = app.db.conn();
        let mut stmt = conn.prepare("SELECT target, content FROM service_files WHERE service_id = ?1 ORDER BY target")?;
        stmt.query_map([service_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    rows.into_iter()
        .map(|(t, c)| Ok((t.clone(), app.keys.open_str(&format!("file:{service_id}:{t}"), &c)?)))
        .collect()
}

pub fn set_file(app: &App, service_id: &str, target: &str, content: Option<&str>) -> Result<()> {
    match content {
        Some(c) => app.db.conn().execute(
            "INSERT INTO service_files (service_id, target, content) VALUES (?1, ?2, ?3) ON CONFLICT(service_id, target) DO UPDATE SET content = excluded.content",
            params![service_id, target, app.keys.seal_str(&format!("file:{service_id}:{target}"), c)?],
        )?,
        None => app.db.conn().execute("DELETE FROM service_files WHERE service_id = ?1 AND target = ?2", params![service_id, target])?,
    };
    Ok(())
}

const DOMAIN_SELECT: &str = "SELECT id, service_id, host, path, port, container, https, via_tunnel, options FROM domains";

fn domain_row(r: &rusqlite::Row) -> rusqlite::Result<Domain> {
    Ok(Domain {
        id: r.get(0)?,
        service_id: r.get(1)?,
        host: r.get(2)?,
        path: r.get(3)?,
        port: r.get::<_, i64>(4)? as u16,
        container: r.get(5)?,
        https: r.get(6)?,
        via_tunnel: r.get(7)?,
        options: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
    })
}

pub fn domains(app: &App, service_id: Option<&str>) -> Result<Vec<Domain>> {
    let conn = app.db.conn();
    let mut stmt = conn.prepare(&format!("{DOMAIN_SELECT} WHERE ?1 IS NULL OR service_id = ?1 ORDER BY host, path"))?;
    Ok(stmt.query_map([service_id], domain_row)?.collect::<rusqlite::Result<_>>()?)
}

pub fn domain(app: &App, id: &str) -> Result<Option<Domain>> {
    Ok(app.db.conn().query_row(&format!("{DOMAIN_SELECT} WHERE id = ?1"), [id], domain_row).optional()?)
}

pub fn valid_host(host: &str) -> bool {
    let h = host.trim_end_matches('.');
    !h.is_empty()
        && h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| {
            !l.is_empty() && l.len() <= 63 && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') && !l.starts_with('-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_keys_and_hosts() {
        assert!(valid_env_key("DATABASE_URL"));
        assert!(valid_env_key("_x1"));
        assert!(!valid_env_key("1X"));
        assert!(!valid_env_key("A-B"));
        assert!(valid_host("shop.example.com"));
        assert!(!valid_host("localhost"));
        assert!(!valid_host("bad_host.com"));
        assert!(!valid_host("-a.com"));
    }
}
