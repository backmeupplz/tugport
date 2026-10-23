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

pub struct Built {
    pub image: String,
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
}

pub async fn build(app: &AppState, s: &Service, d: &Deploy, log: &DeployLog) -> Result<Built> {
    let permit = match app.build_slots.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            log.line("Waiting for a free build slot");
            app.build_slots.clone().acquire_owned().await?
        }
    };
    let work = app.paths.sources().join(&s.id);
    std::fs::create_dir_all(&work)?;
    let (context, sha, message) = match &s.spec.source {
        Source::Git { provider, repo, branch, path, .. } => {
            let git_ref = d.commit_sha.clone().or(d.git_ref.clone()).unwrap_or_else(|| branch.clone());
            let checkout = work.join("repo");
            let (sha, msg) = crate::git::checkout(app, s, provider.as_deref(), repo, &git_ref, &checkout, &|l| log.line(l)).await?;
            let ctx = crate::util::inside(&checkout, path).filter(|c| c.is_dir()).ok_or_else(|| anyhow!("the build path {path} does not exist in the repository"))?;
            (ctx, Some(sha), Some(msg))
        }
        Source::Dockerfile { content } => {
            let dir = work.join("inline");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("Dockerfile"), content)?;
            (dir, None, None)
        }
        _ => bail!("nothing to build for this source"),
    };
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
            dockerfile(app, s, &context, &file, target.as_deref(), &image, sha.as_deref(), &env, log).await?
        }
        Build::Railpack => railpack(app, s, &context, &image, sha.as_deref(), &env, log).await?,
        Build::Nixpacks => nixpacks(app, s, &context, &image, sha.as_deref(), &env, log).await?,
        Build::Buildpacks { builder } => buildpacks(app, s, &context, &image, builder, &env, log).await?,
        Build::Auto => bail!("no build method could be chosen"),
    }
    drop(permit);
    log.line(&format!("Built {image} in {:.1}s", started.elapsed().as_secs_f32()));
    Ok(Built { image, commit_sha: sha, commit_message: message })
}

/// Build-time env for docker: secrets are read from the process environment, never argv,
/// under neutral names, so a variable called PATH or DOCKER_CONFIG cannot steer docker itself.
fn buildx_base(s: &Service, image: &str, env: &BTreeMap<String, Zeroizing<String>>) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "build", "--progress=plain", "--load", "-t", image])
        .args(["--label", &format!("tugport.service={}", s.id)])
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

/// The systemd slice that caps CPU and memory for every build step, if the installer made it.
fn build_slice() -> Option<&'static str> {
    Path::new("/sys/fs/cgroup/tugport.slice/tugport-builds.slice").exists().then_some("tugport-builds.slice")
}

pub fn declared_args(dockerfile: &str) -> Vec<String> {
    dockerfile
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let rest = l.get(..4).filter(|kw| kw.eq_ignore_ascii_case("ARG ")).map(|_| l[4..].trim_start())?;
            let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())).then_some(name)
        })
        .collect()
}

/// Pass variable `k` as a build arg (`--build-arg NAME` reads the value from the environment).
fn build_arg(cmd: &mut Command, env: &BTreeMap<String, Zeroizing<String>>, k: &str, log: &DeployLog) {
    let Some(v) = env.get(k) else { return };
    if crate::proc::steers_tools(k) {
        log.line(&format!("{k} is not passed as a build argument, because it would change how docker itself runs"));
        return;
    }
    cmd.env(k, v.as_str()).args(["--build-arg", k]);
}

#[allow(clippy::too_many_arguments)]
async fn dockerfile(
    app: &AppState,
    s: &Service,
    context: &Path,
    file: &Path,
    target: Option<&str>,
    image: &str,
    sha: Option<&str>,
    env: &BTreeMap<String, Zeroizing<String>>,
    log: &DeployLog,
) -> Result<()> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let declared = declared_args(&text);
    let mut cmd = buildx_base(s, image, env);
    cmd.arg("-f").arg(file);
    if let Some(t) = target {
        cmd.args(["--target", t]);
    }
    for k in declared.iter().chain(s.spec.build_args.iter()) {
        build_arg(&mut cmd, env, k, log);
    }
    if let Some(sha) = sha {
        cmd.args(["--build-arg", &format!("GIT_SHA={sha}")]);
    }
    cmd.arg(context);
    let _ = app;
    crate::proc::run_streamed(cmd, &|l| log.line(l)).await.context("the build failed")
}

async fn railpack(app: &AppState, s: &Service, context: &Path, image: &str, sha: Option<&str>, env: &BTreeMap<String, Zeroizing<String>>, log: &DeployLog) -> Result<()> {
    let bin = tools::ensure(app, Tool::Railpack, &|l| log.line(l)).await?;
    let out = plan_dir(app, s)?;
    let plan = out.join("railpack-plan.json");
    log.line("Planning the build with Railpack");
    let mut prepare = Command::new(&bin);
    prepare.arg("prepare").arg(context).arg("--plan-out").arg(&plan).arg("--info-out").arg(out.join("railpack-info.json"));
    for (k, _) in crate::proc::tool_env(env) {
        // Names only: Railpack reads values from the environment and turns them into build secrets.
        prepare.arg("--env").arg(k);
    }
    prepare.envs(crate::proc::tool_env(env));
    crate::proc::run_streamed(prepare, &|l| log.line(l)).await.context("railpack could not plan this repository")?;
    let mut cmd = buildx_base(s, image, env);
    cmd.args(["--build-arg", &format!("BUILDKIT_SYNTAX={}", tools::railpack_frontend())]);
    cmd.args(["--build-arg", &format!("secrets-hash={}", secrets_hash(env))]);
    if let Some(sha) = sha {
        cmd.args(["--build-arg", &format!("GIT_SHA={sha}")]);
    }
    cmd.arg("-f").arg(&plan).arg(context);
    crate::proc::run_streamed(cmd, &|l| log.line(l)).await.context("the build failed")
}

/// Changes when any variable changes, so cached layers that used it are rebuilt.
fn secrets_hash(env: &BTreeMap<String, Zeroizing<String>>) -> String {
    let joined: String = env.iter().map(|(k, v)| format!("{k}={}\n", v.as_str())).collect();
    crate::util::sha256_hex(joined)
}

fn plan_dir(app: &AppState, s: &Service) -> Result<PathBuf> {
    let dir = app.paths.sources().join(&s.id).join("plan");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

async fn nixpacks(app: &AppState, s: &Service, context: &Path, image: &str, sha: Option<&str>, env: &BTreeMap<String, Zeroizing<String>>, log: &DeployLog) -> Result<()> {
    let bin = tools::ensure(app, Tool::Nixpacks, &|l| log.line(l)).await?;
    log.line("Planning the build with Nixpacks");
    // `--out` writes .nixpacks/Dockerfile without building. Variables are passed by name
    // so values stay out of the generated files; buildx supplies them as build args.
    let mut prepare = Command::new(&bin);
    prepare.arg("build").arg(context).arg("--out").arg(context).arg("--name").arg(image);
    for (k, _) in crate::proc::tool_env(env) {
        prepare.arg("--env").arg(k);
    }
    prepare.envs(crate::proc::tool_env(env));
    crate::proc::run_streamed(prepare, &|l| log.line(l)).await.context("nixpacks could not plan this repository")?;
    let dockerfile = context.join(".nixpacks").join("Dockerfile");
    if !dockerfile.exists() {
        bail!("nixpacks did not produce a Dockerfile");
    }
    let mut cmd = buildx_base(s, image, env);
    cmd.arg("-f").arg(&dockerfile);
    for k in declared_args(&std::fs::read_to_string(&dockerfile).unwrap_or_default()) {
        build_arg(&mut cmd, env, &k, log);
    }
    if let Some(sha) = sha {
        cmd.args(["--build-arg", &format!("GIT_SHA={sha}")]);
    }
    cmd.arg(context);
    crate::proc::run_streamed(cmd, &|l| log.line(l)).await.context("the build failed")
}

async fn buildpacks(app: &AppState, s: &Service, context: &Path, image: &str, builder: &str, env: &BTreeMap<String, Zeroizing<String>>, log: &DeployLog) -> Result<()> {
    let bin = tools::ensure(app, Tool::Pack, &|l| log.line(l)).await?;
    log.line(&format!("Building with buildpacks ({builder})"));
    // Values go through a memory-only env file instead of the command line.
    let env_file = app.paths.run.join(format!("build-{}.env", s.id));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&env_file)?;
        for (k, v) in env {
            writeln!(f, "{k}={}", v.replace('\n', "\\n"))?;
        }
    }
    let mut cmd = Command::new(&bin);
    cmd.args(["build", image, "--builder", builder, "--docker-host", "inherit", "--pull-policy", "if-not-present"]);
    // A trusted builder runs with access to the Docker daemon, so only well-known builders get it;
    // others run their buildpacks in separate containers without it (slower, same result).
    if trusted_builder(builder) {
        cmd.arg("--trust-builder");
    }
    cmd
        .arg("--path")
        .arg(context)
        .arg("--env-file")
        .arg(&env_file);
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
