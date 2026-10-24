//! Reading and writing projects, services, env vars, files and domains.

use crate::app::App;
use crate::model::{DomainOptions, ServiceSpec};
use crate::util;
use anyhow::{Context, Result};
use rusqlite::params;
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

const PROJECT_SELECT: &str = "SELECT id, name, description, created_at FROM projects";

fn project_row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project { id: r.get(0)?, name: r.get(1)?, description: r.get(2)?, created_at: r.get(3)? })
}

pub fn project(app: &App, id_or_name: &str) -> Result<Option<Project>> {
    Ok(app.db.row(&format!("{PROJECT_SELECT} WHERE id = ?1 OR name = ?1"), [id_or_name], project_row)?)
}

pub fn projects(app: &App) -> Result<Vec<Project>> {
    Ok(app.db.rows(&format!("{PROJECT_SELECT} ORDER BY name"), [], project_row)?)
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
    let row = match reference.split_once('.') {
        Some((p, s)) => app.db.row(&format!("{SERVICE_SELECT} WHERE p.name = ?1 AND s.name = ?2"), params![p, s], service_row)?,
        None => app.db.row(&format!("{SERVICE_SELECT} WHERE s.id = ?1"), [reference], service_row)?,
    };
    row.map(with_spec).transpose()
}

pub fn services(app: &App, project_id: Option<&str>) -> Result<Vec<Service>> {
    let rows = app.db.rows(&format!("{SERVICE_SELECT} WHERE ?1 IS NULL OR s.project_id = ?1 ORDER BY p.name, s.name"), [project_id], service_row)?;
    rows.into_iter().map(with_spec).collect()
}

pub fn save_spec(app: &App, service_id: &str, spec: &ServiceSpec) -> Result<()> {
    app.db.exec("UPDATE services SET spec = ?1, updated_at = ?2 WHERE id = ?3", params![serde_json::to_string(spec)?, util::now(), service_id])?;
    Ok(())
}

/// A table of sealed values that belong to a service or project. Each value is sealed with the
/// context `<prefix>:<owner>:<key>`, so a ciphertext copied into another row fails to open.
struct Sealed {
    table: &'static str,
    owner: &'static str,
    key: &'static str,
    value: &'static str,
    prefix: &'static str,
}

const ENV: Sealed = Sealed { table: "service_env", owner: "service_id", key: "key", value: "value", prefix: "env" };
const VARS: Sealed = Sealed { table: "project_vars", owner: "project_id", key: "key", value: "value", prefix: "var" };
const FILES: Sealed = Sealed { table: "service_files", owner: "service_id", key: "target", value: "content", prefix: "file" };

impl Sealed {
    fn context(&self, owner: &str, key: &str) -> String {
        format!("{}:{owner}:{key}", self.prefix)
    }

    /// Every key with its opened value, in key order.
    fn all(&self, app: &App, owner: &str) -> Result<Vec<(String, Zeroizing<String>)>> {
        let Sealed { table, owner: col, key, value, .. } = self;
        let sql = format!("SELECT {key}, {value} FROM {table} WHERE {col} = ?1 ORDER BY {key}");
        let rows: Vec<(String, Vec<u8>)> = app.db.rows(&sql, [owner], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.into_iter().map(|(k, v)| Ok((k.clone(), app.keys.open_str(&self.context(owner, &k), &v)?))).collect()
    }

    /// Set a value, or delete it with None.
    fn set(&self, app: &App, owner: &str, k: &str, v: Option<&str>) -> Result<()> {
        let Sealed { table, owner: col, key, value, .. } = self;
        match v {
            Some(v) => app.db.exec(
                &format!("INSERT INTO {table} ({col}, {key}, {value}) VALUES (?1, ?2, ?3) ON CONFLICT({col}, {key}) DO UPDATE SET {value} = excluded.{value}"),
                params![owner, k, app.keys.seal_str(&self.context(owner, k), v)?],
            )?,
            None => app.db.exec(&format!("DELETE FROM {table} WHERE {col} = ?1 AND {key} = ?2"), params![owner, k])?,
        };
        Ok(())
    }
}

/// Every variable a service's containers get: project variables, overridden by the service's own.
pub fn resolved_env(app: &App, service: &Service) -> Result<BTreeMap<String, Zeroizing<String>>> {
    let mut out = project_vars(app, &service.project_id)?;
    out.extend(service_env(app, &service.id)?);
    Ok(out)
}

pub fn service_env(app: &App, service_id: &str) -> Result<BTreeMap<String, Zeroizing<String>>> {
    Ok(ENV.all(app, service_id)?.into_iter().collect())
}

pub fn env_names(app: &App, service_id: &str) -> Result<Vec<String>> {
    Ok(app.db.rows("SELECT key FROM service_env WHERE service_id = ?1 ORDER BY key", [service_id], |r| r.get(0))?)
}

pub fn set_env(app: &App, service_id: &str, key: &str, value: Option<&str>) -> Result<()> {
    ENV.set(app, service_id, key, value)
}

pub fn project_vars(app: &App, project_id: &str) -> Result<BTreeMap<String, Zeroizing<String>>> {
    Ok(VARS.all(app, project_id)?.into_iter().collect())
}

pub fn set_project_var(app: &App, project_id: &str, key: &str, value: Option<&str>) -> Result<()> {
    VARS.set(app, project_id, key, value)
}

pub fn valid_env_key(key: &str) -> bool {
    let b = key.as_bytes();
    b.first().is_some_and(|c| !c.is_ascii_digit()) && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

pub fn files(app: &App, service_id: &str) -> Result<Vec<(String, Zeroizing<String>)>> {
    FILES.all(app, service_id)
}

pub fn set_file(app: &App, service_id: &str, target: &str, content: Option<&str>) -> Result<()> {
    FILES.set(app, service_id, target, content)
}

const DOMAIN_SELECT: &str = "SELECT id, service_id, host, path, port, container, https, via_tunnel, options FROM domains";

fn domain_row(r: &rusqlite::Row) -> rusqlite::Result<Domain> {
    Ok(Domain {
        id: r.get(0)?,
        service_id: r.get(1)?,
        host: r.get(2)?,
        path: r.get(3)?,
        port: r.get(4)?,
        container: r.get(5)?,
        https: r.get(6)?,
        via_tunnel: r.get(7)?,
        options: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
    })
}

pub fn domains(app: &App, service_id: Option<&str>) -> Result<Vec<Domain>> {
    Ok(app.db.rows(&format!("{DOMAIN_SELECT} WHERE ?1 IS NULL OR service_id = ?1 ORDER BY host, path"), [service_id], domain_row)?)
}

pub fn domain(app: &App, id: &str) -> Result<Option<Domain>> {
    Ok(app.db.row(&format!("{DOMAIN_SELECT} WHERE id = ?1"), [id], domain_row)?)
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
        assert!(!valid_env_key(""));
        assert!(valid_host("shop.example.com"));
        assert!(!valid_host("localhost"));
        assert!(!valid_host("bad_host.com"));
        assert!(!valid_host("-a.com"));
    }

    #[test]
    fn sealed_contexts_stay_the_same() {
        // Stored ciphertexts are bound to these; changing one makes every value unreadable.
        assert_eq!(ENV.context("s1", "KEY"), "env:s1:KEY");
        assert_eq!(VARS.context("p1", "KEY"), "var:p1:KEY");
        assert_eq!(FILES.context("s1", "/etc/a.conf"), "file:s1:/etc/a.conf");
    }
}
