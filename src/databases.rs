//! Database presets: images, credentials, data directories, health checks, dumps.

use crate::model::Engine;
use serde::Deserialize;

/// What Tugport knows about an engine. Shell commands run inside the container with its env loaded.
pub struct Preset {
    pub repo: &'static str,
    fallback_version: &'static str,
    pub port: u16,
    pub health: &'static str,
    /// Dump to stdout (backups) and restore from stdin.
    pub dump: &'static str,
    pub restore: &'static str,
    /// Prints how many tables, collections or keys there are (checks a test restore).
    pub count: &'static str,
    /// The dump's file name in a backup snapshot.
    pub dump_file: &'static str,
    /// The env var with the app user's password.
    pub password_var: &'static str,
}

pub fn preset(engine: Engine) -> Preset {
    match engine {
        Engine::Postgres => Preset {
            repo: "postgres", fallback_version: "18", port: 5432,
            health: "pg_isready -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -h 127.0.0.1",
            dump: "pg_dump -Fc -U \"$POSTGRES_USER\" \"$POSTGRES_DB\"",
            restore: "pg_restore --clean --if-exists --no-owner -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\"",
            count: "psql -U \"$POSTGRES_USER\" -d \"$POSTGRES_DB\" -tAc \"select count(*) from information_schema.tables where table_schema not in ('pg_catalog','information_schema')\"",
            dump_file: "dump.pgc", password_var: "POSTGRES_PASSWORD",
        },
        Engine::Mysql => Preset {
            repo: "mysql", fallback_version: "9", port: 3306,
            health: "mysqladmin ping -h 127.0.0.1 -uroot -p\"$MYSQL_ROOT_PASSWORD\" --silent",
            dump: "mysqldump --single-transaction --routines --triggers --all-databases -uroot -p\"$MYSQL_ROOT_PASSWORD\"",
            restore: "mysql -uroot -p\"$MYSQL_ROOT_PASSWORD\"",
            count: "mysql -uroot -p\"$MYSQL_ROOT_PASSWORD\" -N -e \"select count(*) from information_schema.tables where table_schema not in ('mysql','sys','information_schema','performance_schema')\"",
            dump_file: "dump.sql", password_var: "MYSQL_PASSWORD",
        },
        Engine::Mariadb => Preset {
            repo: "mariadb", fallback_version: "11", port: 3306,
            health: "healthcheck.sh --connect --innodb_initialized",
            dump: "mariadb-dump --single-transaction --routines --triggers --all-databases -uroot -p\"$MARIADB_ROOT_PASSWORD\"",
            restore: "mariadb -uroot -p\"$MARIADB_ROOT_PASSWORD\"",
            count: "mariadb -uroot -p\"$MARIADB_ROOT_PASSWORD\" -N -e \"select count(*) from information_schema.tables where table_schema not in ('mysql','sys','information_schema','performance_schema')\"",
            dump_file: "dump.sql", password_var: "MARIADB_PASSWORD",
        },
        Engine::Mongo => Preset {
            repo: "mongo", fallback_version: "8", port: 27017,
            health: "mongosh --quiet --eval 'db.adminCommand(\"ping\").ok' || mongo --quiet --eval 'db.adminCommand(\"ping\").ok'",
            dump: "mongodump --archive --gzip -u \"$MONGO_INITDB_ROOT_USERNAME\" -p \"$MONGO_INITDB_ROOT_PASSWORD\" --authenticationDatabase admin",
            restore: "mongorestore --archive --gzip --drop -u \"$MONGO_INITDB_ROOT_USERNAME\" -p \"$MONGO_INITDB_ROOT_PASSWORD\" --authenticationDatabase admin",
            count: "mongosh --quiet -u \"$MONGO_INITDB_ROOT_USERNAME\" -p \"$MONGO_INITDB_ROOT_PASSWORD\" --authenticationDatabase admin --eval 'db.adminCommand({listDatabases:1}).databases.length'",
            dump_file: "dump.archive.gz", password_var: "MONGO_INITDB_ROOT_PASSWORD",
        },
        Engine::Redis => Preset {
            repo: "redis", fallback_version: "8", port: 6379,
            health: "redis-cli -a \"$REDIS_PASSWORD\" --no-auth-warning ping | grep -q PONG",
            dump: "redis-cli -a \"$REDIS_PASSWORD\" --no-auth-warning --rdb /tmp/tugport-dump.rdb >/dev/null && cat /tmp/tugport-dump.rdb && rm -f /tmp/tugport-dump.rdb",
            // Redis restores by replacing the RDB file while the server is stopped (the backup module refuses it in place).
            restore: "cat > /data/dump.rdb",
            count: "redis-cli -a \"$REDIS_PASSWORD\" --no-auth-warning dbsize",
            dump_file: "dump.rdb", password_var: "REDIS_PASSWORD",
        },
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

/// Connection string other services in the project use.
pub fn url(engine: Engine, host: &str, user: &str, password: &str, database: &str) -> String {
    let p = preset(engine).port;
    let pw = crate::util::urlencode(password);
    match engine {
        Engine::Postgres => format!("postgres://{user}:{pw}@{host}:{p}/{database}"),
        Engine::Mysql | Engine::Mariadb => format!("mysql://{user}:{pw}@{host}:{p}/{database}"),
        Engine::Mongo => format!("mongodb://{user}:{pw}@{host}:{p}/?authSource=admin"),
        Engine::Redis => format!("redis://default:{pw}@{host}:{p}"),
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
    let tags: Option<Tags> = async { crate::util::http(15).build().ok()?.get(&url).send().await.ok()?.error_for_status().ok()?.json().await.ok() }.await;
    tags.and_then(|t| newest_stable(t.results.iter().map(|t| t.name.as_str()))).unwrap_or_else(|| p.fallback_version.to_string())
}

fn newest_stable<'a>(tags: impl Iterator<Item = &'a str>) -> Option<String> {
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
