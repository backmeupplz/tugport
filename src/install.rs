//! `tugport install`: everything after `curl -fsSL https://tugport.com/install | sh`.

use crate::paths::Paths;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::process::Command;

#[derive(clap::Args)]
pub struct InstallArgs {
    /// Answer yes to every question (for scripts).
    #[arg(long)]
    pub yes: bool,
    /// Skip offering to migrate an existing Easypanel or Coolify.
    #[arg(long)]
    pub no_migrate: bool,
    /// Add this server to an existing panel (the panel shows the full command).
    #[arg(long, requires = "token", value_name = "PANEL_URL")]
    pub join: Option<String>,
    /// One-time token for --join.
    #[arg(long, requires = "join")]
    pub token: Option<String>,
}

fn ask(yes: bool, question: &str) -> bool {
    if yes {
        return true;
    }
    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    let a = line.trim().to_lowercase();
    a.is_empty() || a == "y" || a == "yes"
}

fn ok(cmd: &mut Command) -> bool {
    cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} failed", cmd.get_program());
    }
    Ok(())
}

fn has(program: &str) -> bool {
    ok(Command::new("sh").args(["-c", &format!("command -v {program}")]))
}

fn os_id() -> String {
    std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("ID=").map(|v| v.trim_matches('"').to_string()))
        .unwrap_or_default()
}

fn install_packages(pkgs: &[&str]) -> Result<()> {
    if has("apt-get") {
        run(Command::new("apt-get").args(["update", "-qq"]))?;
        run(Command::new("apt-get").args(["install", "-y", "-qq"]).args(pkgs).env("DEBIAN_FRONTEND", "noninteractive"))
    } else if has("dnf") {
        run(Command::new("dnf").args(["install", "-y", "-q"]).args(pkgs))
    } else if has("apk") {
        run(Command::new("apk").args(["add", "--no-cache"]).args(pkgs))
    } else {
        bail!("install {} with your package manager and run the installer again", pkgs.join(", "))
    }
}

pub fn install(args: InstallArgs) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("run the installer as root (sudo)");
    }
    println!("Installing Tugport {} on {}.", crate::VERSION, if os_id().is_empty() { "this server".into() } else { os_id() });

    if !ok(Command::new("docker").arg("version")) {
        if !ask(args.yes, "Docker is not installed. Install it now with the official script from get.docker.com?") {
            bail!("Tugport needs Docker");
        }
        run(Command::new("sh").args(["-c", "curl -fsSL --retry 5 https://get.docker.com | sh"]))?;
        let _ = ok(Command::new("systemctl").args(["enable", "--now", "docker"]));
    }
    let mut missing = vec![];
    if !ok(Command::new("docker").args(["buildx", "version"])) {
        missing.push("docker-buildx-plugin");
    }
    if !ok(Command::new("docker").args(["compose", "version"])) {
        missing.push("docker-compose-plugin");
    }
    for (bin, pkg) in [("git", "git"), ("ssh-keygen", "openssh-client"), ("wg", "wireguard-tools")] {
        if !has(bin) {
            missing.push(pkg);
        }
    }
    if !missing.is_empty() {
        println!("Installing {}.", missing.join(", "));
        install_packages(&missing)?;
    }

    let paths = Paths::from_env();
    paths.ensure()?;
    let exe = std::env::current_exe()?;
    let target = std::path::Path::new("/usr/local/bin/tugport");
    if exe != target {
        std::fs::copy(&exe, target).context("copying the binary to /usr/local/bin")?;
        crate::paths::restrict(target, 0o755)?;
    }
    if let (Some(panel), Some(token)) = (&args.join, &args.token) {
        return join(panel, token);
    }
    let has_key = paths.etc.join("master.key").exists() || paths.etc.join("master.key.cred").exists();
    if !has_key {
        let (key, store) = crate::crypto::create_master(&paths.etc)?;
        let where_ = match store {
            crate::crypto::KeyStore::Tpm => "sealed to this server's TPM",
            crate::crypto::KeyStore::File => "in /etc/tugport/master.key",
        };
        println!("\nYour secrets are encrypted with a master key kept {where_}.");
        println!("Write down this recovery key and keep it somewhere safe. It is shown only once, and it is the");
        println!("only way to read your secrets and backups if this server's disk or TPM is lost:\n");
        println!("    {}\n", crate::crypto::recovery_key(&key));
        if !args.yes {
            print!("Press Enter once you have saved it. ");
            let _ = std::io::stdout().flush();
            let mut s = String::new();
            let _ = std::io::stdin().lock().read_line(&mut s);
        }
    }

    write_units()?;
    run(Command::new("systemctl").arg("daemon-reload"))?;
    run(Command::new("systemctl").args(["enable", "--now", "tugport.service"]))?;
    if ok(Command::new("systemctl").args(["is-active", "--quiet", "tugport.service"])) {
        let _ = ok(Command::new("systemctl").args(["reload", "tugport.service"]));
    }

    let found = tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(crate::migrate::detect());
    for f in &found {
        let name = if f.panel == "easypanel" { "Easypanel" } else { "Coolify" };
        println!("\nFound {name} {} on this server.", f.version);
    }
    if !args.no_migrate && !found.is_empty() {
        println!("Tugport can move it over service by service while your apps keep running. See the plan first with:");
        for f in &found {
            println!("    tugport migrate {} --dry-run", f.panel);
        }
    }

    // Wait for the panel, then print the setup link.
    for _ in 0..50 {
        if std::net::TcpStream::connect("127.0.0.1:8484").is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let db = crate::db::Db::open(&paths.db())?;
    match crate::setup_link(&db)? {
        Some(link) => println!("\nTugport is running. Open {link} to create your account."),
        None => println!("\nTugport is running and already set up; open http://{}:8484 to sign in.", crate::public_address().unwrap_or_else(|| "this-server".into())),
    }
    if ok(Command::new("sh").args(["-c", "ufw status | grep -q 'Status: active'"])) {
        println!("ufw is active: allow the panel and web traffic with `ufw allow 80,443,8484/tcp`.");
    }
    Ok(())
}

/// Become a server of an existing panel: mesh, overlay networks, then the agent service.
fn join(panel: &str, token: &str) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(crate::agent::join(panel, token))?;
    let unit = "[Unit]\n\
Description=Tugport node\n\
Documentation=https://tugport.com\n\
After=network-online.target docker.service\n\
Wants=network-online.target\n\
Requires=docker.service\n\
\n\
[Service]\n\
ExecStart=/usr/local/bin/tugport node\n\
Restart=always\n\
RestartSec=2\n\
Environment=RUST_LOG=tugport=info\n\
LimitNOFILE=1048576\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n";
    std::fs::write("/etc/systemd/system/tugport-node.service", unit)?;
    run(Command::new("systemctl").arg("daemon-reload"))?;
    run(Command::new("systemctl").args(["enable", "--now", "tugport-node.service"]))?;
    println!("\nThis server joined {panel} as {} on the private network. It shows up under Servers within a minute.", config.mesh_ip);
    if ok(Command::new("sh").args(["-c", "ufw status | grep -q 'Status: active'"])) {
        println!("ufw is active: allow the mesh and web traffic with `ufw allow 51820/udp && ufw allow 80,443/tcp`.");
    }
    Ok(())
}

fn write_units() -> Result<()> {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let mem_kb: u64 = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:").and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse().ok()))
        .unwrap_or(4 * 1024 * 1024);
    // Builds together get half the machine, so they can never starve running services.
    let slice = format!(
        "[Unit]\nDescription=Tugport builds\n\n[Slice]\nCPUQuota={}%\nMemoryMax={}K\nMemorySwapMax=0\n",
        cores.div_ceil(2) * 100,
        mem_kb / 2
    );
    let service = "[Unit]\n\
Description=Tugport\n\
Documentation=https://tugport.com\n\
After=network-online.target docker.service\n\
Wants=network-online.target\n\
Requires=docker.service\n\
\n\
[Service]\n\
ExecStart=/usr/local/bin/tugport server\n\
ExecReload=/bin/kill -HUP $MAINPID\n\
Restart=always\n\
RestartSec=2\n\
Environment=RUST_LOG=tugport=info\n\
LimitNOFILE=1048576\n\
KillMode=mixed\n\
TimeoutStopSec=40\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n";
    std::fs::write("/etc/systemd/system/tugport-builds.slice", slice)?;
    std::fs::write("/etc/systemd/system/tugport.service", service)?;
    Ok(())
}
