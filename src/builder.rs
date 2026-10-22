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
use anyhow::{Context, Result, bail};
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
            let ctx = if path.trim_matches('/').is_empty() { checkout } else { checkout.join(path.trim_matches('/')) };
            if !ctx.starts_with(work.join("repo")) || !ctx.is_dir() {
                bail!("the build path {path} does not exist in the repository");
            }
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
            let file = context.join(path);
            if !file.starts_with(&context) || !file.exists() {
                bail!("{path} not found in the build context");
            }
            dockerfile(app, s, &context, &file, target.as_deref(), &image, sha.as_deref(), &env, log).await?
        }
        Build::Railpack => railpack(app, s, &context, &image, sha.as_deref(), &env, log).await?,
        Build::Nixpacks => nixpacks(app, s, &context, &image, sha.as_deref(), &env, log).await?,
        Build::Buildpacks { builder } => buildpacks(app, s, &context, &image, builder, &env, log).await?,
        Build::Auto => unreachable!(),
    }
    drop(permit);
    log.line(&format!("Built {image} in {:.1}s", started.elapsed().as_secs_f32()));
    Ok(Built { image, commit_sha: sha, commit_message: message })
}

/// Build-time env for docker: secrets are read from the process environment, never argv.
fn buildx_base(s: &Service, image: &str, env: &BTreeMap<String, Zeroizing<String>>) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(["buildx", "build", "--progress=plain", "--load", "-t", image])
        .args(["--label", &format!("tugport.service={}", s.id)])
        .env("DOCKER_BUILDKIT", "1");
    for (k, v) in env {
        cmd.arg("--secret").arg(format!("id={k},env={k}"));
        cmd.env(k, v.as_str());
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
    let re = regex::Regex::new(r"(?mi)^\s*ARG\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    re.captures_iter(dockerfile).map(|c| c[1].to_string()).collect()
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
        if env.contains_key(k) {
            // `--build-arg NAME` without a value reads it from the environment.
            cmd.args(["--build-arg", k]);
        }
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
    for k in env.keys() {
        // Names only: Railpack reads values from the environment and turns them into build secrets.
        prepare.arg("--env").arg(k);
    }
    prepare.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
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
    for k in env.keys() {
        prepare.arg("--env").arg(k);
    }
    prepare.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    crate::proc::run_streamed(prepare, &|l| log.line(l)).await.context("nixpacks could not plan this repository")?;
    let dockerfile = context.join(".nixpacks").join("Dockerfile");
    if !dockerfile.exists() {
        bail!("nixpacks did not produce a Dockerfile");
    }
    let mut cmd = buildx_base(s, image, env);
    cmd.arg("-f").arg(&dockerfile);
    for k in declared_args(&std::fs::read_to_string(&dockerfile).unwrap_or_default()) {
        if env.contains_key(&k) {
            cmd.args(["--build-arg", &k]);
        }
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
    cmd.args(["build", image, "--builder", builder, "--trust-builder", "--docker-host", "inherit", "--pull-policy", "if-not-present"])
        .arg("--path")
        .arg(context)
        .arg("--env-file")
        .arg(&env_file);
    let res = crate::proc::run_streamed(cmd, &|l| log.line(l)).await.context("the buildpack build failed");
    let _ = std::fs::remove_file(&env_file);
    res
}

#[cfg(test)]
mod tests {
    use super::declared_args;

    #[test]
    fn finds_declared_args() {
        let df = "FROM node:24\nARG NPM_TOKEN\n arg VITE_API_URL=default\nRUN echo $NPM_TOKEN\n# ARG COMMENTED\n";
        assert_eq!(declared_args(df), vec!["NPM_TOKEN", "VITE_API_URL"]);
    }
}
