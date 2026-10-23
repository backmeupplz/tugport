//! Database read replicas. A replica is a database service of its own (`<db>-replica-N`)
//! that follows its primary over the project network, can run on any server or region,
//! and can be promoted when the primary is lost.
//!
//! - Postgres: streaming replication, seeded by pg_basebackup on first start.
//! - MySQL and MariaDB: binlog replication, seeded from a dump of the primary.
//! - MongoDB: the primary becomes a one-member replica set; replicas join it without votes,
//!   so the primary keeps working when they are away.
//! - Redis: `replicaof`.

use crate::app::{App, AppState};
use crate::model::{DatabaseSpec, Engine, Mount, Placement, Source};
use crate::runtime::{self, Replica};
use crate::store::{self, Service};
use crate::{databases, deploy, util};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use std::time::Duration;

const KEYFILE: &str = "/etc/tugport/mongo-keyfile";

/// Mongo with access control needs a shared key file, readable only by mongod.
const MONGO_REPLSET: &str = "cp /etc/tugport/mongo-keyfile /tmp/tugport-keyfile && chmod 400 /tmp/tugport-keyfile \
    && (chown mongodb:mongodb /tmp/tugport-keyfile 2>/dev/null || chown 999:999 /tmp/tugport-keyfile) \
    && exec docker-entrypoint.sh mongod --replSet rs0 --keyFile /tmp/tugport-keyfile --bind_ip_all";

const PG_REPLICA: &str = r#"set -e
if [ ! -s "$PGDATA/PG_VERSION" ]; then
  mkdir -p "$PGDATA" && chown -R postgres:postgres "$PGDATA" && chmod 700 "$PGDATA"
  as=gosu; command -v gosu >/dev/null 2>&1 || as=su-exec
  until $as postgres env PGPASSWORD="$POSTGRES_PASSWORD" pg_basebackup -h "$PRIMARY_HOST" -U "$POSTGRES_USER" -D "$PGDATA" -R -X stream -c fast; do
    echo "waiting for the primary at $PRIMARY_HOST"; rm -rf "${PGDATA:?}"/*; sleep 5
  done
fi
exec docker-entrypoint.sh postgres"#;

const PG_ALLOW: &str = r#"f="$PGDATA/pg_hba.conf"
grep -q '^host replication all all' "$f" || echo 'host replication all all scram-sha-256' >> "$f"
psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc 'select pg_reload_conf()'"#;

const REDIS_REPLICA: &str = r#"if [ -f /data/.tugport-primary ]; then exec redis-server --requirepass "$REDIS_PASSWORD" --appendonly yes; fi
exec redis-server --requirepass "$REDIS_PASSWORD" --masterauth "$REDIS_PASSWORD" --replicaof "$PRIMARY_HOST" 6379 --appendonly yes"#;

/// Root's password, quoted for SQL (`'` doubled).
const SQL_PW: &str = r#"pw=$(printf %s "${MYSQL_ROOT_PASSWORD:-$MARIADB_ROOT_PASSWORD}" | sed "s/'/''/g")"#;

fn engine(s: &Service) -> Result<Engine> {
    s.spec.database.as_ref().map(|d| d.engine).ok_or_else(|| anyhow!("{} is not a database", s.name))
}

/// The running container of a database's live version.
async fn container(app: &App, s: &Service) -> Result<Replica> {
    let live = runtime::live_deploy(app, &s.id);
    runtime::replicas(app, &s.id)
        .await?
        .into_iter()
        .find(|r| r.state == "running" && live.as_deref().is_none_or(|l| r.deploy == l))
        .ok_or_else(|| anyhow!("{} is not running", s.name))
}

async fn run(app: &App, r: &Replica, script: &str) -> Result<String> {
    let (code, out) = runtime::exec(app, r, script).await?;
    if code != 0 {
        bail!("{} (exit {code}): {}", r.name, out.trim());
    }
    Ok(out)
}

/// Run a command until it prints `want` (replication catching up, a node electing itself).
async fn until(app: &App, r: &Replica, script: &str, want: &str, timeout: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        if runtime::exec(app, r, script).await.is_ok_and(|(code, out)| code == 0 && out.contains(want)) {
            return Ok(());
        }
        if started.elapsed() > timeout {
            bail!("{} did not report {want} within {}s", r.name, timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn mongo_shell(js: &str, user_var: &str, pw_var: &str) -> String {
    format!("mongosh --quiet -u \"${user_var}\" -p \"${pw_var}\" --authenticationDatabase admin --eval '{js}'")
}

/// Replicas of a database.
pub fn of(app: &App, primary_id: &str) -> Result<Vec<Service>> {
    Ok(store::services(app, None)?.into_iter().filter(|s| s.spec.database.as_ref().and_then(|d| d.primary.as_deref()) == Some(primary_id)).collect())
}

/// Wait for a deploy to finish; an error unless it went live.
async fn live(app: &App, deploy_id: &str) -> Result<()> {
    loop {
        let d = deploy::get(app, deploy_id)?.ok_or_else(|| anyhow!("the deploy disappeared"))?;
        match d.status.as_str() {
            "live" => return Ok(()),
            s if deploy::FINAL.contains(&s) => bail!("the deploy ended {s}: {}", d.error.unwrap_or_default()),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

async fn redeploy(app: &AppState, s: &Service, why: &str) -> Result<()> {
    let image = runtime::live_deploy(app, &s.id).and_then(|d| deploy::get(app, &d).ok().flatten()).and_then(|d| d.image);
    let id = deploy::enqueue(app, s, deploy::Request { trigger: why.into(), image, ..Default::default() })?;
    live(app, &id).await
}

fn save_spec(app: &App, s: &Service) -> Result<()> {
    app.db.conn().execute("UPDATE services SET spec = ?1, updated_at = ?2 WHERE id = ?3", params![serde_json::to_string(&s.spec)?, util::now(), s.id])?;
    Ok(())
}

/// Get the primary ready to be followed (once per database).
async fn prepare_primary(app: &AppState, p: &Service, eng: Engine) -> Result<()> {
    let c = container(app, p).await?;
    match eng {
        Engine::Postgres => {
            run(app, &c, PG_ALLOW).await.context("allowing replication connections")?;
        }
        Engine::Redis => {}
        Engine::Mysql => {
            let on = run(app, &c, r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql -uroot -N -e 'select @@log_bin'"#).await?;
            if on.trim() != "1" {
                bail!("binary logging is off on {}; replicas need it", p.name);
            }
        }
        Engine::Mariadb => {
            let on = run(app, &c, r#"MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -N -e 'select @@log_bin'"#).await?;
            if on.trim() != "1" {
                tracing::info!("turning on binary logging for {}/{} (a short restart)", p.project, p.name);
                let mut s = p.clone();
                s.spec.command = Some(["mariadbd", "--log-bin=mariadb-bin", "--server-id=1", "--binlog-format=ROW"].map(String::from).to_vec());
                save_spec(app, &s)?;
                redeploy(app, &s, "replication").await?;
            }
        }
        Engine::Mongo => {
            let set = run(app, &c, &mongo_shell("db.hello().setName || \"none\"", "MONGO_INITDB_ROOT_USERNAME", "MONGO_INITDB_ROOT_PASSWORD")).await?;
            if set.trim() == "rs0" {
                return Ok(());
            }
            tracing::info!("making {}/{} a replica set (a short restart)", p.project, p.name);
            let key = crate::crypto::b64(&rand::random::<[u8; 512]>());
            store::set_file(app, &p.id, KEYFILE, Some(&key))?;
            let mut s = p.clone();
            s.spec.entrypoint = Some(vec!["sh".into(), "-c".into(), MONGO_REPLSET.into()]);
            s.spec.command = None;
            save_spec(app, &s)?;
            redeploy(app, &s, "replication").await?;
            let c = container(app, &s).await?;
            let init = format!("rs.initiate({{_id: \"rs0\", members: [{{_id: 0, host: \"{}:27017\"}}]}}).ok", s.name);
            run(app, &c, &mongo_shell(&init, "MONGO_INITDB_ROOT_USERNAME", "MONGO_INITDB_ROOT_PASSWORD")).await.context("starting the replica set")?;
            let hello = mongo_shell("db.hello().isWritablePrimary", "MONGO_INITDB_ROOT_USERNAME", "MONGO_INITDB_ROOT_PASSWORD");
            until(app, &c, &hello, "true", Duration::from_secs(60)).await?;
        }
    }
    Ok(())
}

/// Add a read replica, placed like any service. Returns it and its first deploy; it
/// starts following the primary once that deploy is live.
pub async fn add(app: &AppState, primary: &Service, placement: Placement) -> Result<(Service, String)> {
    let eng = engine(primary)?;
    let db = primary.spec.database.clone().ok_or_else(|| anyhow!("not a database"))?;
    if db.primary.is_some() {
        bail!("{} is itself a replica; add replicas to its primary", primary.name);
    }
    let Source::Image { image } = &primary.spec.source else { bail!("databases run from an image") };
    let image = image.clone();
    prepare_primary(app, primary, eng).await?;
    let primary = store::service(app, &primary.id)?.ok_or_else(|| anyhow!("the primary was deleted"))?;
    let taken: Vec<String> = store::services(app, Some(&primary.project_id))?.into_iter().map(|s| s.name).collect();
    let name = (1..100).map(|i| format!("{}-replica-{i}", primary.name)).find(|n| !taken.contains(n)).ok_or_else(|| anyhow!("too many replicas"))?;
    if !util::valid_name(&name) {
        bail!("the replica's name {name} is too long; give the database a shorter name");
    }

    // Unless told otherwise, a replica goes to a different server than its primary.
    let placement = if placement == Placement::default() {
        let home = crate::nodes::home(app, &primary)?.id.clone();
        let other = crate::nodes::eligible(app, &Placement::default()).into_iter().find(|n| n.id != home);
        other.map(|n| Placement { node: Some(n.id.clone()), ..Default::default() }).unwrap_or(placement)
    } else {
        placement
    };
    let mut spec = primary.spec.clone();
    spec.placement = placement;
    spec.aliases.clear();
    spec.ports.clear();
    spec.autoscale = None;
    spec.sleep = None;
    // The first copy of a large database takes a while.
    spec.rollout.health_timeout_s = 3600;
    spec.database = Some(DatabaseSpec { primary: Some(primary.id.clone()), ..db });
    let version = image.rsplit_once(':').map(|(_, v)| v).unwrap_or_default();
    let data = databases::data_dir(eng, version);
    spec.mounts.retain(|m| m.target() != data);
    spec.mounts.push(Mount::Volume { name: format!("tp-{}-{name}-data", primary.project), target: data.into(), readonly: false });
    let server_id = rand::random::<u32>() % 2_000_000_000 + 2;
    let sh = |script: &str| Some(vec!["sh".to_string(), "-c".to_string(), script.to_string()]);
    (spec.entrypoint, spec.command) = match eng {
        Engine::Postgres => (sh(PG_REPLICA), None),
        Engine::Redis => (None, sh(REDIS_REPLICA)),
        Engine::Mysql => (None, Some(vec!["mysqld".into(), format!("--server-id={server_id}"), "--relay-log=relay-bin".into()])),
        Engine::Mariadb => (
            sh(&format!(
                "ro=1; [ -f /var/lib/mysql/.tugport-primary ] && ro=0\nexec docker-entrypoint.sh mariadbd --server-id={server_id} --relay-log=relay-bin --log-bin=mariadb-bin --binlog-format=ROW --read-only=$ro"
            )),
            None,
        ),
        Engine::Mongo => (sh(MONGO_REPLSET), None),
    };

    let id = util::id();
    let now = util::now();
    app.db.conn().execute(
        "INSERT INTO services (id, project_id, name, kind, spec, created_at, updated_at) VALUES (?1, ?2, ?3, 'database', ?4, ?5, ?5)",
        params![id, primary.project_id, name, serde_json::to_string(&spec)?, now],
    )?;
    // Same credentials as the primary, plus where to find it.
    for (k, v) in store::service_env(app, &primary.id)? {
        let k = match (eng, k.as_str()) {
            // Users come from the primary; a replica must not create its own.
            (Engine::Mongo, "MONGO_INITDB_ROOT_USERNAME") => "MONGO_USER".to_string(),
            (Engine::Mongo, "MONGO_INITDB_ROOT_PASSWORD") => "MONGO_PASSWORD".to_string(),
            _ => k,
        };
        store::set_env(app, &id, &k, Some(&v))?;
    }
    store::set_env(app, &id, "PRIMARY_HOST", Some(&primary.name))?;
    for (target, content) in store::files(app, &primary.id)? {
        store::set_file(app, &id, &target, Some(&content))?;
    }
    let replica = store::service(app, &id)?.ok_or_else(|| anyhow!("the replica was not saved"))?;
    let deploy_id = deploy::enqueue(app, &replica, deploy::Request { trigger: "replica".into(), image: Some(image), ..Default::default() })?;
    app.db.set_state(&format!("replica:{id}"), "copying")?;
    let (app2, p, r, d) = (app.clone(), primary.id.clone(), id.clone(), deploy_id.clone());
    tokio::spawn(async move { follow(app2, p, r, d).await });
    Ok((replica, deploy_id))
}

async fn follow(app: AppState, primary_id: String, replica_id: String, deploy_id: String) {
    let result = async {
        live(&app, &deploy_id).await?;
        let primary = store::service(&app, &primary_id)?.ok_or_else(|| anyhow!("the primary was deleted"))?;
        let replica = store::service(&app, &replica_id)?.ok_or_else(|| anyhow!("the replica was deleted"))?;
        attach(&app, &primary, &replica).await?;
        publish_urls(&app, &primary)
    }
    .await;
    let name = store::service(&app, &replica_id).ok().flatten();
    match result {
        Ok(()) => {
            let _ = app.db.set_state(&format!("replica:{replica_id}"), "following");
            if let Some(s) = &name {
                tracing::info!("{}/{} follows its primary", s.project, s.name);
                app.emit("service.status", Some(&s.project_id), Some(&s.id), serde_json::json!({ "event": "replica", "replica": "following" }));
            }
        }
        Err(e) => {
            let _ = app.db.set_state(&format!("replica:{replica_id}"), &format!("failed: {e:#}"));
            if let Some(s) = &name {
                tracing::warn!("{}/{} could not start following its primary: {e:#}", s.project, s.name);
                crate::notify::event(&app, "deploy.failed", &format!("{}/{} is not replicating", s.project, s.name), &format!("{e:#}"), Some(s)).await;
            }
        }
    }
}

/// Start following, once the replica runs.
async fn attach(app: &AppState, primary: &Service, replica: &Service) -> Result<()> {
    let eng = engine(primary)?;
    let src = container(app, primary).await?;
    let dst = container(app, replica).await?;
    let wait = Duration::from_secs(300);
    match eng {
        Engine::Postgres => until(app, &dst, r#"psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc 'select pg_is_in_recovery()'"#, "t", wait).await,
        Engine::Redis => until(app, &dst, r#"redis-cli -a "$REDIS_PASSWORD" --no-auth-warning info replication"#, "master_link_status:up", wait).await,
        Engine::Mysql | Engine::Mariadb => {
            let maria = eng == Engine::Mariadb;
            let (dump, load) = if maria {
                (
                    r#"MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb-dump -uroot --all-databases --single-transaction --routines --triggers --events --master-data=1"#,
                    r#"MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot"#,
                )
            } else {
                (
                    r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysqldump -uroot --all-databases --single-transaction --routines --triggers --events --source-data=1"#,
                    r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql -uroot"#,
                )
            };
            let mut producer = crate::nodes::docker_cli(app, &src.node)?;
            producer.args(["exec", &src.id, crate::secrets::INIT_PATH, "exec", "--", "sh", "-c", dump]);
            let mut consumer = crate::nodes::docker_cli(app, &dst.node)?;
            consumer.args(["exec", "-i", &dst.id, crate::secrets::INIT_PATH, "exec", "--", "sh", "-c", load]);
            crate::backup::pipe(producer, consumer).await.context("copying the primary's data")?;
            let start = if maria {
                format!(
                    "{SQL_PW}\nMYSQL_PWD=\"$MARIADB_ROOT_PASSWORD\" mariadb -uroot -e \"CHANGE MASTER TO MASTER_HOST='$PRIMARY_HOST', MASTER_PORT=3306, MASTER_USER='root', MASTER_PASSWORD='$pw'; START SLAVE;\""
                )
            } else {
                format!(
                    "{SQL_PW}\nMYSQL_PWD=\"$MYSQL_ROOT_PASSWORD\" mysql -uroot -e \"CHANGE REPLICATION SOURCE TO SOURCE_HOST='$PRIMARY_HOST', SOURCE_PORT=3306, SOURCE_USER='root', SOURCE_PASSWORD='$pw', GET_SOURCE_PUBLIC_KEY=1; START REPLICA; SET PERSIST read_only = ON; SET PERSIST super_read_only = ON;\""
                )
            };
            run(app, &dst, &start).await.context("starting replication")?;
            let status = if maria {
                r#"MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -E -e 'SHOW SLAVE STATUS'"#
            } else {
                r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql -uroot -E -e 'SHOW REPLICA STATUS'"#
            };
            until(app, &dst, status, if maria { "Slave_IO_Running: Yes" } else { "Replica_IO_Running: Yes" }, wait).await
        }
        Engine::Mongo => {
            let add = format!("rs.add({{host: \"{}:27017\", priority: 0, votes: 0}}).ok", replica.name);
            run(app, &src, &mongo_shell(&add, "MONGO_INITDB_ROOT_USERNAME", "MONGO_INITDB_ROOT_PASSWORD")).await.context("adding the replica to the set")?;
            let state = format!("rs.status().members.filter(m => m.name == \"{}:27017\").map(m => m.stateStr).join()", replica.name);
            until(app, &src, &mongo_shell(&state, "MONGO_INITDB_ROOT_USERNAME", "MONGO_INITDB_ROOT_PASSWORD"), "SECONDARY", Duration::from_secs(3600)).await
        }
    }
}

fn password_var(eng: Engine) -> &'static str {
    match eng {
        Engine::Postgres => "POSTGRES_PASSWORD",
        Engine::Mysql => "MYSQL_PASSWORD",
        Engine::Mariadb => "MARIADB_PASSWORD",
        Engine::Mongo => "MONGO_INITDB_ROOT_PASSWORD",
        Engine::Redis => "REDIS_PASSWORD",
    }
}

fn var_name(service: &str, suffix: &str) -> String {
    format!("{}_{suffix}", service.to_uppercase().replace('-', "_"))
}

/// `<DB>_READ_URL` for the project (all replicas for Mongo, the first one otherwise) and
/// each replica's own `<REPLICA>_URL`.
fn publish_urls(app: &App, primary: &Service) -> Result<()> {
    let eng = engine(primary)?;
    let db = primary.spec.database.clone().ok_or_else(|| anyhow!("not a database"))?;
    let pw = store::service_env(app, &primary.id)?.get(password_var(eng)).map(|p| p.to_string()).unwrap_or_default();
    let mut replicas: Vec<Service> = of(app, &primary.id)?.into_iter().filter(|r| app.db.get_state(&format!("replica:{}", r.id)).ok().flatten().is_none_or(|s| !s.starts_with("failed"))).collect();
    replicas.sort_by(|a, b| a.name.cmp(&b.name));
    for r in &replicas {
        store::set_project_var(app, &primary.project_id, &var_name(&r.name, "URL"), Some(&databases::url(eng, &r.name, &db.user, &pw, &db.database)))?;
    }
    let read = match (eng, replicas.first()) {
        (_, None) => None,
        (Engine::Mongo, Some(_)) => {
            let hosts: Vec<String> = std::iter::once(&primary.name).chain(replicas.iter().map(|r| &r.name)).map(|h| format!("{h}:27017")).collect();
            let full = databases::url(eng, "HOST", &db.user, &pw, &db.database);
            let creds = full.trim_start_matches("mongodb://").split('@').next().unwrap_or_default().to_string();
            Some(format!("mongodb://{creds}@{}/?replicaSet=rs0&readPreference=secondaryPreferred&authSource=admin", hosts.join(",")))
        }
        (_, Some(first)) => Some(databases::url(eng, &first.name, &db.user, &pw, &db.database)),
    };
    store::set_project_var(app, &primary.project_id, &var_name(&primary.name, "READ_URL"), read.as_deref())?;
    Ok(())
}

/// Make a replica a primary of its own (the old primary is lost or being retired).
/// Returns the project variable that now points at it.
pub async fn promote(app: &AppState, replica: &Service) -> Result<String> {
    let eng = engine(replica)?;
    let primary_id = replica.spec.database.as_ref().and_then(|d| d.primary.clone()).ok_or_else(|| anyhow!("{} is not a replica", replica.name))?;
    let c = container(app, replica).await?;
    let script = match eng {
        Engine::Postgres => r#"psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc 'select pg_promote()'"#.to_string(),
        Engine::Redis => r#"redis-cli -a "$REDIS_PASSWORD" --no-auth-warning replicaof no one && touch /data/.tugport-primary"#.into(),
        Engine::Mysql => r#"MYSQL_PWD="$MYSQL_ROOT_PASSWORD" mysql -uroot -e 'STOP REPLICA; RESET REPLICA ALL; SET PERSIST super_read_only = OFF; SET PERSIST read_only = OFF;'"#.into(),
        Engine::Mariadb => {
            r#"MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -e 'STOP SLAVE; RESET SLAVE ALL; SET GLOBAL read_only = OFF;' && touch /var/lib/mysql/.tugport-primary"#.into()
        }
        Engine::Mongo => {
            let js = format!(
                "cfg = rs.conf(); cfg.members = cfg.members.filter(m => m.host == \"{}:27017\"); cfg.members[0].priority = 1; cfg.members[0].votes = 1; rs.reconfig(cfg, {{force: true}}).ok",
                replica.name
            );
            mongo_shell(&js, "MONGO_USER", "MONGO_PASSWORD")
        }
    };
    run(app, &c, &script).await.context("promoting")?;
    let mut s = replica.clone();
    if let Some(db) = s.spec.database.as_mut() {
        db.primary = None;
    }
    save_spec(app, &s)?;
    app.db.conn().execute("DELETE FROM state WHERE key = ?1", [format!("replica:{}", s.id)])?;
    // The old primary's URL variable now leads here.
    let old = store::service(app, &primary_id)?;
    let var = var_name(old.as_ref().map(|p| p.name.as_str()).unwrap_or(&s.name), "URL");
    let db = s.spec.database.clone().ok_or_else(|| anyhow!("not a database"))?;
    let pw_var = if eng == Engine::Mongo { "MONGO_PASSWORD" } else { password_var(eng) };
    let pw = store::service_env(app, &s.id)?.get(pw_var).map(|p| p.to_string()).unwrap_or_default();
    store::set_project_var(app, &s.project_id, &var, Some(&databases::url(eng, &s.name, &db.user, &pw, &db.database)))?;
    if let Some(old) = &old {
        publish_urls(app, old)?;
    }
    Ok(var)
}

/// Replication state for the panel: copying, following, or failed with the reason.
pub fn state(app: &App, replica_id: &str) -> String {
    app.db.get_state(&format!("replica:{replica_id}")).ok().flatten().unwrap_or_else(|| "following".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_names() {
        assert_eq!(var_name("db-replica-1", "URL"), "DB_REPLICA_1_URL");
        assert_eq!(var_name("db", "READ_URL"), "DB_READ_URL");
    }
}
