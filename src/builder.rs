//! Building images: Dockerfile (BuildKit), Railpack, Nixpacks, buildpacks.
//!
//! Builds use the Docker daemon's BuildKit, so layer and cache-mount caches persist
//! between deploys. Env vars reach builds as BuildKit secrets; only names the
//! Dockerfile declares as ARG (or listed in `build_args`) are passed as build args.

use crate::app::AppState;
use crate::deploy::{Deploy, DeployLog};
use crate::model::{Build, Source};
use crate::store::{self, Service};
use crate::tools::{self, Tool};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use zeroize::Zeroizing;

type Env = BTreeMap<String, Zeroizing<String>>;

/// Build the deploy's image and return its name.
pub async fn build(app: &AppState, s: &Service, d: &Deploy, log: &DeployLog) -> Result<String> {
    let _permit = match app.build_slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            log.line("Waiting for a free build slot");
            app.build_slots.clone().acquire_owned().await?
        }
    };
    let (context, sha) = match &s.spec.source {
        Source::Git { .. } => {
            let (dir, sha) = crate::git::checkout(app, s, d, &|l| log.line(l)).await?;
            (dir, Some(sha))
        }
        Source::Dockerfile { content } => (write_inline(app, s, "Dockerfile", content)?.0, None),
        _ => bail!("nothing to build for this source"),
    };
    let sha = sha.as_deref();
    let image = format!("{}:{}", s.slug(), d.id);
    let env = store::resolved_env(app, s)?;
    let method = match &s.spec.build {
        Build::Auto if context.join("Dockerfile").exists() => Build::Dockerfile { path: "Dockerfile".into(), target: None },
        Build::Auto => Build::Railpack,
        other => other.clone(),
    };
    let started = std::time::Instant::now();
    match &method {
        Build::Dockerfile { path, target } => {
            log.line(&format!("Building with {path}"));
            let file = crate::util::inside(&context, path).filter(|f| f.is_file()).ok_or_else(|| anyhow!("{path} not found in the build context"))?;
            let mut cmd = buildx(s, &image, &env, &file);
            if let Some(t) = target {
                cmd.args(["--target", t]);
            }
            build_args(&mut cmd, &env, &file, &s.spec.build_args, log);
            run(cmd, &context, sha, log).await?
        }
        Build::Railpack => railpack(app, s, &context, &image, sha, &env, log).await?,
        Build::Nixpacks => nixpacks(app, s, &context, &image, sha, &env, log).await?,
        Build::Buildpacks { builder } => buildpacks(app, s, &context, &image, builder, &env, log).await?,
        Build::Auto => bail!("no build method could be chosen"),
    }
    log.line(&format!("Built {image} in {:.1}s", started.elapsed().as_secs_f32()));
    Ok(image)
}

/// `docker buildx build -f file`. Build-time env for docker: secrets are read from
/// the process environment, never argv, under neutral names, so a variable called PATH or
/// DOCKER_CONFIG cannot steer docker itself.
fn buildx(s: &Service, image: &str, env: &Env, file: &Path) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "build", "--progress=plain", "--load", "-t", image])
        .args(["--label", &format!("tugport.service={}", s.id)])
        .arg("-f")
        .arg(file)
        .env("DOCKER_BUILDKIT", "1");
    for (i, (k, v)) in env.iter().enumerate() {
        let alias = format!("TUGPORT_BUILD_SECRET_{i}");
        cmd.arg("--secret").arg(format!("id={k},env={alias}"));
        cmd.env(alias, v.as_str());
    }
    if let Some(slice) = build_slice() {
        cmd.arg(format!("--cgroup-parent={slice}"));
    }
    cmd
}

/// A source pasted into the service (Dockerfile, Compose YAML) written to its own folder:
/// returns (folder, file).
pub fn write_inline(app: &AppState, s: &Service, name: &str, content: &str) -> Result<(PathBuf, PathBuf)> {
    let dir = app.paths.sources().join(&s.id).join("inline");
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(name);
    std::fs::write(&file, content)?;
    Ok((dir, file))
}

/// Run a buildx command on `context`. GIT_SHA goes last, so it wins over a variable of that name.
async fn run(mut cmd: Command, context: &Path, sha: Option<&str>, log: &DeployLog) -> Result<()> {
    if let Some(sha) = sha {
        cmd.args(["--build-arg", &format!("GIT_SHA={sha}")]);
    }
    cmd.arg(context);
    crate::proc::run_streamed(cmd, &|l| log.line(l)).await.context("the build failed")
}

/// The systemd slice that caps CPU and memory for every build step, if the installer made it.
fn build_slice() -> Option<&'static str> {
    Path::new("/sys/fs/cgroup/tugport.slice/tugport-builds.slice").exists().then_some("tugport-builds.slice")
}

fn declared_args(dockerfile: &str) -> Vec<String> {
    dockerfile
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let rest = l.get(..4).filter(|kw| kw.eq_ignore_ascii_case("ARG ")).and(l.get(4..))?.trim_start();
            let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())).then_some(name)
        })
        .collect()
}

/// Pass the variables `dockerfile` declares as ARG, and `extra`, as build args (`--build-arg NAME`
/// reads the value from the environment).
fn build_args(cmd: &mut Command, env: &Env, dockerfile: &Path, extra: &[String], log: &DeployLog) {
    for k in declared_args(&std::fs::read_to_string(dockerfile).unwrap_or_default()).iter().chain(extra) {
        let Some(v) = env.get(k) else { continue };
        if crate::proc::steers_tools(k) {
            log.line(&format!("{k} is not passed as a build argument, because it would change how docker itself runs"));
            continue;
        }
        cmd.env(k, v.as_str()).args(["--build-arg", k]);
    }
}

/// Run a planner (railpack, nixpacks) with the variables' names as `--env`: it reads their values
/// from its environment.
async fn plan(mut cmd: Command, env: &Env, tool: &str, log: &DeployLog) -> Result<()> {
    log.line(&format!("Planning the build with {tool}"));
    for (k, _) in crate::proc::tool_env(env) {
        cmd.arg("--env").arg(k);
    }
    cmd.envs(crate::proc::tool_env(env));
    crate::proc::run_streamed(cmd, &|l| log.line(l)).await.with_context(|| format!("{} could not plan this repository", tool.to_lowercase()))
}

async fn railpack(app: &AppState, s: &Service, context: &Path, image: &str, sha: Option<&str>, env: &Env, log: &DeployLog) -> Result<()> {
    let bin = tools::ensure(app, Tool::Railpack, &|l| log.line(l)).await?;
    let out = app.paths.sources().join(&s.id).join("plan");
    std::fs::create_dir_all(&out)?;
    let plan_file = out.join("railpack-plan.json");
    let mut prepare = Command::new(&bin);
    prepare.arg("prepare").arg(context).arg("--plan-out").arg(&plan_file).arg("--info-out").arg(out.join("railpack-info.json"));
    // Railpack turns the variables into build secrets.
    plan(prepare, env, "Railpack", log).await?;
    let mut cmd = buildx(s, image, env, &plan_file);
    cmd.args(["--build-arg", &format!("BUILDKIT_SYNTAX={}", tools::RAILPACK_FRONTEND)]);
    cmd.args(["--build-arg", &format!("secrets-hash={}", secrets_hash(env))]);
    run(cmd, context, sha, log).await
}

/// Changes when any variable changes, so cached layers that used it are rebuilt.
fn secrets_hash(env: &Env) -> String {
    let joined = Zeroizing::new(env.iter().map(|(k, v)| format!("{k}={}\n", v.as_str())).collect::<String>());
    crate::util::sha256_hex(joined.as_bytes())
}

async fn nixpacks(app: &AppState, s: &Service, context: &Path, image: &str, sha: Option<&str>, env: &Env, log: &DeployLog) -> Result<()> {
    let bin = tools::ensure(app, Tool::Nixpacks, &|l| log.line(l)).await?;
    // `--out` writes .nixpacks/Dockerfile without building. Variables are passed by name
    // so values stay out of the generated files; buildx supplies them as build args.
    let mut prepare = Command::new(&bin);
    prepare.arg("build").arg(context).arg("--out").arg(context).arg("--name").arg(image);
    plan(prepare, env, "Nixpacks", log).await?;
    let dockerfile = context.join(".nixpacks").join("Dockerfile");
    if !dockerfile.exists() {
        bail!("nixpacks did not produce a Dockerfile");
    }
    let mut cmd = buildx(s, image, env, &dockerfile);
    build_args(&mut cmd, env, &dockerfile, &[], log);
    run(cmd, context, sha, log).await
}

async fn buildpacks(app: &AppState, s: &Service, context: &Path, image: &str, builder: &str, env: &Env, log: &DeployLog) -> Result<()> {
    let bin = tools::ensure(app, Tool::Pack, &|l| log.line(l)).await?;
    log.line(&format!("Building with buildpacks ({builder})"));
    // Values go through a memory-only env file instead of the command line.
    let env_file = app.paths.run.join(format!("build-{}.env", s.id));
    let lines = Zeroizing::new(env.iter().map(|(k, v)| format!("{k}={}\n", v.replace('\n', "\\n"))).collect::<String>());
    crate::util::write_private(&env_file, lines.as_bytes())?;
    let mut cmd = Command::new(&bin);
    cmd.args(["build", image, "--builder", builder, "--docker-host", "inherit", "--pull-policy", "if-not-present"]);
    // A trusted builder runs with access to the Docker daemon, so only well-known builders get it;
    // others run their buildpacks in separate containers without it (slower, same result).
    if trusted_builder(builder) {
        cmd.arg("--trust-builder");
    }
    cmd.arg("--path").arg(context).arg("--env-file").arg(&env_file);
    let res = crate::proc::run_streamed(cmd, &|l| log.line(l)).await.context("the buildpack build failed");
    let _ = std::fs::remove_file(&env_file);
    res
}

/// Builders published by the buildpacks projects themselves.
fn trusted_builder(builder: &str) -> bool {
    let b = builder.trim_start_matches("docker.io/").trim_start_matches("index.docker.io/");
    ["heroku/builder:", "paketobuildpacks/builder:", "paketobuildpacks/builder-jammy-", "paketobuildpacks/ubuntu-noble-builder", "gcr.io/buildpacks/builder"]
        .iter()
        .any(|p| b.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::{declared_args, trusted_builder};

    #[test]
    fn only_known_builders_are_trusted() {
        assert!(trusted_builder("heroku/builder:24"));
        assert!(trusted_builder("docker.io/paketobuildpacks/builder-jammy-base"));
        assert!(!trusted_builder("example/builder:24"));
        assert!(!trusted_builder("heroku/builder-evil:1"));
    }

    #[test]
    fn finds_declared_args() {
        let df = "FROM node:24\nARG NPM_TOKEN\n arg VITE_API_URL=default\nRUN echo $NPM_TOKEN\n# ARG COMMENTED\n";
        assert_eq!(declared_args(df), vec!["NPM_TOKEN", "VITE_API_URL"]);
    }
}
