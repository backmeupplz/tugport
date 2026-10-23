//! Who is calling and what they may do. Humans sign in with a password (+ TOTP);
//! agents only ever hold API keys. Permissions are per-project roles, an optional
//! role on all projects (including future ones), and two extra grants.

use crate::db::Db;
use crate::util;
use anyhow::{Result, anyhow};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Status, charts and logs.
    Viewer = 1,
    /// Also deploy, restart, stop and roll back.
    Deployer = 2,
    /// Also change env, domains, mounts and create services.
    Operator = 3,
    /// Also delete things and manage project members.
    Admin = 4,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Deployer => "deployer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

/// Lets the holder read secret values back (writing never needs it).
pub const GRANT_SECRETS_READ: &str = "secrets:read";
/// Lets the holder delete services, volumes, databases and backups.
pub const GRANT_DESTROY: &str = "destroy";
pub const GRANTS: &[&str] = &[GRANT_SECRETS_READ, GRANT_DESTROY];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UserKind {
    Human,
    Agent,
}

#[derive(Clone, Debug, Serialize)]
pub struct Principal {
    pub user_id: String,
    pub name: String,
    pub kind: UserKind,
    pub is_owner: bool,
    pub all_projects_role: Option<Role>,
    pub project_roles: HashMap<String, Role>,
    pub grants: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip)]
    pub ip: Option<String>,
}

impl Principal {
    pub fn role_in(&self, project_id: &str) -> Option<Role> {
        if self.is_owner {
            return Some(Role::Admin);
        }
        self.project_roles.get(project_id).copied().max(self.all_projects_role)
    }

    pub fn can(&self, project_id: &str, need: Role) -> bool {
        self.role_in(project_id).is_some_and(|r| r >= need)
    }

    pub fn has_grant(&self, grant: &str) -> bool {
        self.is_owner || self.grants.iter().any(|g| g == grant)
    }

    pub fn can_destroy(&self, project_id: &str) -> bool {
        self.can(project_id, Role::Admin) && self.has_grant(GRANT_DESTROY)
    }

    pub fn can_read_secrets(&self, project_id: &str) -> bool {
        self.can(project_id, Role::Operator) && self.has_grant(GRANT_SECRETS_READ)
    }

    /// Projects this principal can see at all; None means every project.
    pub fn visible_projects(&self) -> Option<Vec<String>> {
        if self.is_owner || self.all_projects_role.is_some() {
            None
        } else {
            Some(self.project_roles.keys().cloned().collect())
        }
    }
}

pub fn load_principal(db: &Db, user_id: &str) -> Result<Option<Principal>> {
    let conn = db.conn();
    let row = conn
        .query_row(
            "SELECT id, name, kind, is_owner, all_projects_role, grants, disabled FROM users WHERE id = ?1",
            [user_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((id, name, kind, is_owner, all_role, grants, disabled)) = row else { return Ok(None) };
    if disabled {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT project_id, role FROM project_access WHERE user_id = ?1")?;
    let project_roles = stmt
        .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .filter_map(|(p, role)| Role::parse(&role).map(|role| (p, role)))
        .collect();
    Ok(Some(Principal {
        user_id: id,
        name,
        kind: if kind == "agent" { UserKind::Agent } else { UserKind::Human },
        is_owner,
        all_projects_role: all_role.as_deref().and_then(Role::parse),
        project_roles,
        grants: serde_json::from_str(&grants).unwrap_or_default(),
        key_id: None,
        ip: None,
    }))
}

pub fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let salt = SaltString::generate(&mut OsRng);
    Ok(argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hashing password: {e}"))?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    PasswordHash::new(hash)
        .map(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

/// Argon2 is slow on purpose. It runs off the async threads, which the proxy shares with the
/// panel, and only a few at a time, so a flood of wrong passwords cannot stall every site.
pub async fn hashing<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let _slot = SLOTS.acquire().await.ok()?;
    tokio::task::spawn_blocking(f).await.ok()
}

pub const SESSION_COOKIE: &str = "tp_session";
const SESSION_DAYS: i64 = 30;

pub fn create_session(db: &Db, user_id: &str, ip: Option<&str>, user_agent: Option<&str>) -> Result<String> {
    let token = util::token(43);
    let now = util::now();
    db.conn().execute(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at, ip, user_agent) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![util::sha256_hex(&token), user_id, now, now + SESSION_DAYS * 86400, ip, user_agent],
    )?;
    Ok(token)
}

pub fn session_user(db: &Db, token: &str) -> Result<Option<String>> {
    let now = util::now();
    let conn = db.conn();
    let hash = util::sha256_hex(token);
    let user: Option<(String, i64)> = conn
        .query_row("SELECT user_id, expires_at FROM sessions WHERE token_hash = ?1", [&hash], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    Ok(match user {
        Some((uid, exp)) if exp > now => {
            // Sliding expiry, refreshed at most daily.
            if exp - now < (SESSION_DAYS - 1) * 86400 {
                conn.execute(
                    "UPDATE sessions SET expires_at = ?1 WHERE token_hash = ?2",
                    params![now + SESSION_DAYS * 86400, hash],
                )?;
            }
            Some(uid)
        }
        Some(_) => {
            conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [&hash])?;
            None
        }
        None => None,
    })
}

pub fn delete_session(db: &Db, token: &str) -> Result<()> {
    db.conn().execute("DELETE FROM sessions WHERE token_hash = ?1", [util::sha256_hex(token)])?;
    Ok(())
}

pub const KEY_PREFIX: &str = "tpk_";

pub struct NewKey {
    pub id: String,
    pub token: String,
}

pub fn create_key(
    db: &Db,
    user_id: &str,
    name: &str,
    expires_at: Option<i64>,
    allowed_ips: &[ipnet::IpNet],
) -> Result<NewKey> {
    let token = format!("{KEY_PREFIX}{}", util::token(40));
    let id = util::id();
    let ips = if allowed_ips.is_empty() { None } else { Some(serde_json::to_string(allowed_ips)?) };
    db.conn().execute(
        "INSERT INTO api_keys (id, user_id, name, prefix, hash, expires_at, allowed_ips, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, user_id, name, &token[..12], util::sha256_hex(&token), expires_at, ips, util::now()],
    )?;
    Ok(NewKey { id, token })
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyCheck {
    Ok { user_id: String, key_id: String },
    Unknown,
    Expired,
    Revoked,
    IpNotAllowed,
}

#[allow(clippy::type_complexity)]
pub fn check_key(db: &Db, token: &str, ip: Option<IpAddr>) -> Result<KeyCheck> {
    if !token.starts_with(KEY_PREFIX) {
        return Ok(KeyCheck::Unknown);
    }
    let conn = db.conn();
    let row: Option<(String, String, Option<i64>, Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT id, user_id, expires_at, allowed_ips, revoked_at FROM api_keys WHERE hash = ?1",
            [util::sha256_hex(token)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((key_id, user_id, expires, ips, revoked)) = row else { return Ok(KeyCheck::Unknown) };
    if revoked.is_some() {
        return Ok(KeyCheck::Revoked);
    }
    let now = util::now();
    if expires.is_some_and(|e| e <= now) {
        return Ok(KeyCheck::Expired);
    }
    if let Some(ips) = ips {
        let nets: Vec<ipnet::IpNet> = serde_json::from_str(&ips).unwrap_or_default();
        if !ip.is_some_and(|ip| nets.iter().any(|n| n.contains(&ip))) {
            return Ok(KeyCheck::IpNotAllowed);
        }
    }
    conn.execute(
        "UPDATE api_keys SET last_used_at = ?1, last_used_ip = ?2 WHERE id = ?3",
        params![now, ip.map(|i| i.to_string()), key_id],
    )?;
    Ok(KeyCheck::Ok { user_id, key_id })
}

/// TOTP for humans; the secret is stored encrypted.
pub fn totp(secret: &[u8], account: &str) -> Result<totp_rs::TOTP> {
    totp_rs::TOTP::new(totp_rs::Algorithm::SHA1, 6, 1, 30, secret.to_vec(), Some("Tugport".into()), account.into())
        .map_err(|e| anyhow!("totp: {e}"))
}

pub fn new_totp_secret() -> Vec<u8> {
    totp_rs::Secret::generate_secret().to_bytes().unwrap_or_else(|_| util::token(20).into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_user(kind: &str, owner: bool) -> (Db, String) {
        let db = Db::memory().unwrap();
        let id = util::id();
        db.conn()
            .execute(
                "INSERT INTO users (id, kind, name, is_owner, created_at) VALUES (?1, ?2, ?3, ?4, 0)",
                params![id, kind, format!("u{id}"), owner],
            )
            .unwrap();
        (db, id)
    }

    #[test]
    fn roles_and_grants() {
        let (db, uid) = db_with_user("agent", false);
        db.conn().execute("INSERT INTO projects (id, name, created_at) VALUES ('p1','shop',0)", []).unwrap();
        db.conn().execute("INSERT INTO projects (id, name, created_at) VALUES ('p2','blog',0)", []).unwrap();
        db.conn()
            .execute("INSERT INTO project_access (user_id, project_id, role) VALUES (?1, 'p1', 'deployer')", [&uid])
            .unwrap();
        let p = load_principal(&db, &uid).unwrap().unwrap();
        assert!(p.can("p1", Role::Viewer));
        assert!(p.can("p1", Role::Deployer));
        assert!(!p.can("p1", Role::Operator));
        assert!(!p.can("p2", Role::Viewer));
        assert!(!p.can_read_secrets("p1"));
        assert!(!p.can_destroy("p1"));

        // "All projects" covers projects that do not exist yet.
        db.conn().execute("UPDATE users SET all_projects_role = 'operator', grants = '[\"secrets:read\"]' WHERE id = ?1", [&uid]).unwrap();
        let p = load_principal(&db, &uid).unwrap().unwrap();
        assert!(p.can("future-project", Role::Operator));
        assert!(p.can_read_secrets("future-project"));
        assert!(!p.can_destroy("p1"), "destroy needs admin role and the grant");
        assert_eq!(p.visible_projects(), None);
    }

    #[test]
    fn owner_can_do_everything() {
        let (db, uid) = db_with_user("human", true);
        let p = load_principal(&db, &uid).unwrap().unwrap();
        assert!(p.can_destroy("anything") && p.can_read_secrets("anything"));
    }

    #[test]
    fn keys_expire_revoke_and_check_ips() {
        let (db, uid) = db_with_user("agent", false);
        let k = create_key(&db, &uid, "ci", None, &[]).unwrap();
        assert!(matches!(check_key(&db, &k.token, None).unwrap(), KeyCheck::Ok { .. }));
        assert_eq!(check_key(&db, "tpk_nope", None).unwrap(), KeyCheck::Unknown);

        let limited = create_key(&db, &uid, "lan", None, &["10.0.0.0/8".parse().unwrap()]).unwrap();
        assert!(matches!(check_key(&db, &limited.token, Some("10.1.2.3".parse().unwrap())).unwrap(), KeyCheck::Ok { .. }));
        assert_eq!(check_key(&db, &limited.token, Some("8.8.8.8".parse().unwrap())).unwrap(), KeyCheck::IpNotAllowed);

        let old = create_key(&db, &uid, "old", Some(util::now() - 1), &[]).unwrap();
        assert_eq!(check_key(&db, &old.token, None).unwrap(), KeyCheck::Expired);

        db.conn().execute("UPDATE api_keys SET revoked_at = 1 WHERE id = ?1", [&k.id]).unwrap();
        assert_eq!(check_key(&db, &k.token, None).unwrap(), KeyCheck::Revoked);
    }

    #[test]
    fn passwords_and_sessions() {
        let h = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
        let (db, uid) = db_with_user("human", false);
        let t = create_session(&db, &uid, None, None).unwrap();
        assert_eq!(session_user(&db, &t).unwrap().as_deref(), Some(uid.as_str()));
        delete_session(&db, &t).unwrap();
        assert_eq!(session_user(&db, &t).unwrap(), None);
    }
}
