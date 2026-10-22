//! Database presets: images, credentials, data directories, health checks, dumps.

use crate::model::Engine;
use serde::Deserialize;

pub struct Preset {
    pub repo: &'static str,
    pub fallback_version: &'static str,
    pub port: u16,
}

pub fn preset(engine: Engine) -> Preset {
    match engine {
        Engine::Postgres => Preset { repo: "postgres", fallback_version: "18", port: 5432 },
        Engine::Mysql => Preset { repo: "mysql", fallback_version: "9", port: 3306 },
        Engine::Mariadb => Preset { repo: "mariadb", fallback_version: "11", port: 3306 },
        Engine::Mongo => Preset { repo: "mongo", fallback_version: "8", port: 27017 },
        Engine::Redis => Preset { repo: "redis", fallback_version: "8", port: 6379 },
    }
}

pub fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Mongo => "mongo",
        Engine::Redis => "redis",
    }
}

fn major(version: &str) -> u32 {
    version.split(['.', '-']).next().and_then(|m| m.parse().ok()).unwrap_or(0)
}

/// Where the data volume is mounted. Postgres 18 images moved PGDATA under /var/lib/postgresql.
pub fn data_dir(engine: Engine, version: &str) -> &'static str {
    match engine {
        Engine::Postgres if major(version) >= 18 => "/var/lib/postgresql",
        Engine::Postgres => "/var/lib/postgresql/data",
        Engine::Mysql | Engine::Mariadb => "/var/lib/mysql",
        Engine::Mongo => "/data/db",
        Engine::Redis => "/data",
    }
}

/// Env vars the official image reads on first start.
pub fn init_env(engine: Engine, user: &str, database: &str, password: &str) -> Vec<(&'static str, String)> {
    match engine {
        Engine::Postgres => vec![
            ("POSTGRES_USER", user.into()),
            ("POSTGRES_PASSWORD", password.into()),
            ("POSTGRES_DB", database.into()),
        ],
        Engine::Mysql => vec![
            ("MYSQL_ROOT_PASSWORD", password.into()),
            ("MYSQL_DATABASE", database.into()),
            ("MYSQL_USER", user.into()),
            ("MYSQL_PASSWORD", password.into()),
        ],
        Engine::Mariadb => vec![
            ("MARIADB_ROOT_PASSWORD", password.into()),
            ("MARIADB_DATABASE", database.into()),
            ("MARIADB_USER", user.into()),
            ("MARIADB_PASSWORD", password.into()),
        ],
        Engine::Mongo => vec![("MONGO_INITDB_ROOT_USERNAME", user.into()), ("MONGO_INITDB_ROOT_PASSWORD", password.into())],
        Engine::Redis => vec![("REDIS_PASSWORD", password.into())],
    }
}

/// Command override (Redis needs its password on the command line).
pub fn command(engine: Engine) -> Option<Vec<String>> {
    match engine {
        Engine::Redis => Some(vec![
            "sh".into(),
            "-c".into(),
            "exec redis-server --requirepass \"$REDIS_PASSWORD\" --appendonly yes".into(),
        ]),
        _ => None,
    }
}

/// Shell health check; runs inside the container with its env loaded.
pub fn health(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "pg_isready -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -h 127.0.0.1",
        Engine::Mysql => "mysqladmin ping -h 127.0.0.1 -uroot -p\"$MYSQL_ROOT_PASSWORD\" --silent",
        Engine::Mariadb => "healthcheck.sh --connect --innodb_initialized",
        Engine::Mongo => "mongosh --quiet --eval 'db.adminCommand(\"ping\").ok' || mongo --quiet --eval 'db.adminCommand(\"ping\").ok'",
        Engine::Redis => "redis-cli -a \"$REDIS_PASSWORD\" --no-auth-warning ping | grep -q PONG",
    }
}

/// Connection string other services in the project use.
pub fn url(engine: Engine, host: &str, user: &str, password: &str, database: &str) -> String {
    let p = preset(engine).port;
    let pw = percent_encode(password);
    match engine {
        Engine::Postgres => format!("postgres://{user}:{pw}@{host}:{p}/{database}"),
        Engine::Mysql | Engine::Mariadb => format!("mysql://{user}:{pw}@{host}:{p}/{database}"),
        Engine::Mongo => format!("mongodb://{user}:{pw}@{host}:{p}/?authSource=admin"),
        Engine::Redis => format!("redis://default:{pw}@{host}:{p}"),
    }
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Dump to stdout (backups) and restore from stdin, both run inside the container.
pub fn dump_command(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "pg_dump -Fc -U \"$POSTGRES_USER\" \"$POSTGRES_DB\"",
        Engine::Mysql => "mysqldump --single-transaction --routines --triggers --all-databases -uroot -p\"$MYSQL_ROOT_PASSWORD\"",
        Engine::Mariadb => "mariadb-dump --single-transaction --routines --triggers --all-databases -uroot -p\"$MARIADB_ROOT_PASSWORD\"",
        Engine::Mongo => "mongodump --archive --gzip -u \"$MONGO_INITDB_ROOT_USERNAME\" -p \"$MONGO_INITDB_ROOT_PASSWORD\" --authenticationDatabase admin",
        Engine::Redis => "redis-cli -a \"$REDIS_PASSWORD\" --no-auth-warning --rdb /tmp/tugport-dump.rdb >/dev/null && cat /tmp/tugport-dump.rdb && rm -f /tmp/tugport-dump.rdb",
    }
}

pub fn restore_command(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "pg_restore --clean --if-exists --no-owner -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\"",
        Engine::Mysql => "mysql -uroot -p\"$MYSQL_ROOT_PASSWORD\"",
        Engine::Mariadb => "mariadb -uroot -p\"$MARIADB_ROOT_PASSWORD\"",
        Engine::Mongo => "mongorestore --archive --gzip --drop -u \"$MONGO_INITDB_ROOT_USERNAME\" -p \"$MONGO_INITDB_ROOT_PASSWORD\" --authenticationDatabase admin",
        // Redis restores by replacing the RDB file while the server is stopped (handled by the backup module).
        Engine::Redis => "cat > /data/dump.rdb",
    }
}

#[derive(Deserialize)]
struct Tags {
    results: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

/// Newest stable version tag on Docker Hub (`18.1`, `9.5.0`), so new databases start pinned.
pub async fn latest_version(engine: Engine) -> String {
    let p = preset(engine);
    let url = format!("https://hub.docker.com/v2/repositories/library/{}/tags?page_size=100&ordering=last_updated", p.repo);
    let tags: Option<Tags> = async { reqwest::get(&url).await.ok()?.json().await.ok() }.await;
    tags.and_then(|t| newest_stable(t.results.iter().map(|t| t.name.as_str()))).unwrap_or_else(|| p.fallback_version.to_string())
}

pub fn newest_stable<'a>(tags: impl Iterator<Item = &'a str>) -> Option<String> {
    tags.filter(|t| t.split('.').count() >= 2 && t.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())))
        .max_by_key(|t| t.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_newest_stable_tag() {
        let tags = ["latest", "18", "18.1", "18.0", "17.6", "18.1-alpine", "19beta1", "9.10", "9.9.1"];
        assert_eq!(newest_stable(tags.iter().copied()).as_deref(), Some("18.1"));
        assert_eq!(newest_stable(["9.10", "9.9.1", "9.2"].into_iter()).as_deref(), Some("9.10"));
    }

    #[test]
    fn data_dirs_and_urls() {
        assert_eq!(data_dir(Engine::Postgres, "18.1"), "/var/lib/postgresql");
        assert_eq!(data_dir(Engine::Postgres, "16"), "/var/lib/postgresql/data");
        assert_eq!(url(Engine::Postgres, "db", "app", "p@ss", "app"), "postgres://app:p%40ss@db:5432/app");
    }
}
