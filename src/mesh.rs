//! The WireGuard mesh between servers: one interface (`tugport0`), one UDP port, every
//! server a peer of every other. Addresses live in 10.99.0.0/16; the panel's server is .1.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Command;

pub const IFACE: &str = "tugport0";
pub const PORT: u16 = 51820;
pub const PANEL_IP: &str = "10.99.0.1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Peer {
    pub public_key: String,
    /// `host:port` it can be reached at; None for peers behind NAT (they connect to us).
    pub endpoint: Option<String>,
    pub mesh_ip: String,
}

/// The address of mesh member `index` (1 = the panel's server).
pub fn mesh_ip(index: u32) -> String {
    format!("10.99.{}.{}", index / 250, index % 250 + if index / 250 == 0 { 0 } else { 1 })
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// New private key and its public key.
pub fn keypair() -> Result<(String, String)> {
    let private = run(Command::new("wg").arg("genkey"))?;
    let mut child = Command::new("wg").arg("pubkey").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
    {
        use std::io::Write;
        child.stdin.take().ok_or_else(|| anyhow::anyhow!("wg pubkey has no stdin"))?.write_all(private.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("wg pubkey failed");
    }
    Ok((private, String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Bring the interface up with our address and private key (idempotent).
pub fn up(private_key_file: &std::path::Path, own_ip: &str) -> Result<()> {
    let exists = Command::new("ip").args(["link", "show", IFACE]).output().map(|o| o.status.success()).unwrap_or(false);
    if !exists {
        run(Command::new("ip").args(["link", "add", IFACE, "type", "wireguard"]))?;
    }
    run(Command::new("wg").args(["set", IFACE, "listen-port", &PORT.to_string(), "private-key"]).arg(private_key_file))?;
    let addrs = run(Command::new("ip").args(["-o", "addr", "show", "dev", IFACE]))?;
    if !addrs.contains(&format!("{own_ip}/16")) {
        run(Command::new("ip").args(["addr", "add", &format!("{own_ip}/16"), "dev", IFACE]))?;
    }
    // VXLAN (Docker overlay) runs inside WireGuard, so leave room for both headers.
    run(Command::new("ip").args(["link", "set", "mtu", "1420", "up", "dev", IFACE]))?;
    Ok(())
}

/// Make the interface's peers exactly `peers` (adds, updates and removes).
pub fn set_peers(peers: &[Peer]) -> Result<()> {
    let current = run(Command::new("wg").args(["show", IFACE, "peers"])).unwrap_or_default();
    for key in current.lines().filter(|k| !k.is_empty()) {
        if !peers.iter().any(|p| p.public_key == key) {
            run(Command::new("wg").args(["set", IFACE, "peer", key, "remove"]))?;
        }
    }
    for p in peers {
        let mut cmd = Command::new("wg");
        cmd.args(["set", IFACE, "peer", &p.public_key, "allowed-ips", &format!("{}/32", p.mesh_ip), "persistent-keepalive", "25"]);
        if let Some(ep) = &p.endpoint {
            cmd.args(["endpoint", ep]);
        }
        run(&mut cmd)?;
    }
    Ok(())
}

/// Containers must never open connections into the mesh: from there they could reach other
/// servers' Docker, agents, proxies (as a trusted hop) and the image registry. Only traffic
/// arriving over WireGuard itself, or from this host, may start a mesh connection.
pub fn guard() -> Result<()> {
    const CHAIN: &str = "TUGPORT-MESH";
    // At boot Docker, firewalls and Tugport may all hold the xtables lock for a moment. Bounded,
    // since a hung holder would otherwise keep Docker from starting (it runs this first).
    let ok = |args: &[&str]| Command::new("iptables").args(["-w", "10"]).args(args).output().map(|o| o.status.success()).unwrap_or(false);
    if !ok(&["-n", "-L", CHAIN]) {
        run(Command::new("iptables").args(["-w", "10", "-N", CHAIN])).context("blocking containers from the mesh")?;
    }
    // In FORWARD the rule would end up below the ACCEPT rules Docker inserts when it starts.
    let forward = if docker_user_chain().is_ok() { "DOCKER-USER" } else { "FORWARD" };
    // Published ports are DNAT-ed before filtering, so match the original destination.
    let rules: [(&str, &str, &[&str]); 5] = [
        ("-A", CHAIN, &["-i", IFACE, "-j", "RETURN"]),
        ("-A", CHAIN, &["-i", "lo", "-j", "RETURN"]),
        ("-A", CHAIN, &["-m", "conntrack", "--ctstate", "NEW", "-j", "DROP"]),
        ("-I", forward, &["-m", "conntrack", "--ctorigdst", "10.99.0.0/16", "-j", CHAIN]),
        ("-I", "INPUT", &["-d", "10.99.0.0/16", "-j", CHAIN]),
    ];
    for (op, chain, rule) in rules {
        if !ok(&[&["-C", chain], rule].concat()) {
            run(Command::new("iptables").args([&["-w", "10", op, chain], rule].concat())).context("blocking containers from the mesh")?;
        }
    }
    Ok(())
}

/// `tugport mesh-up`: the interface and its peers first, then the guard (even when a peer
/// could not be set). Docker is not
/// running yet, so nothing can reach the mesh before the guard is in place, and a failing
/// iptables call must not keep swarm mode from binding its address.
pub fn boot(private_key_file: &std::path::Path, own_ip: &str, peers: &[Peer]) -> Result<()> {
    up(private_key_file, own_ip)?;
    let peers = set_peers(peers);
    guard()?;
    peers
}

/// Docker's DOCKER-USER chain and its jump from FORWARD, created when missing (at boot Docker
/// is not running yet). Docker (27 to 29) keeps an existing chain with its rules and only moves
/// the jump back to the top of FORWARD, so the guard stays in front of Docker's rules. The jump
/// also covers a Docker that never sets up the chain (iptables off, or its nftables backend).
fn docker_user_chain() -> Result<()> {
    const CHAIN: &str = "DOCKER-USER";
    let ok = |args: &[&str]| Command::new("iptables").args(["-w", "10"]).args(args).output().map(|o| o.status.success()).unwrap_or(false);
    if !ok(&["-n", "-L", CHAIN]) {
        run(Command::new("iptables").args(["-w", "10", "-N", CHAIN])).context("creating DOCKER-USER")?;
    }
    if !ok(&["-C", "FORWARD", "-j", CHAIN]) {
        run(Command::new("iptables").args(["-w", "10", "-I", "FORWARD", "-j", CHAIN])).context("creating DOCKER-USER")?;
    }
    Ok(())
}

const DOCKER_DROPIN: &str = "/etc/systemd/system/docker.service.d/tugport-mesh.conf";
const DROPIN: &str = "# Written by Tugport. Swarm mode and the image registry listen on mesh addresses,\n\
# so the mesh has to be up before Docker starts (at boot, Docker starts before Tugport).\n\
[Service]\n\
ExecStartPre=-/usr/local/bin/tugport mesh-up\n";

/// Make Docker bring the mesh up before it starts. Without it, after a reboot swarm mode
/// cannot bind its mesh address and stays broken until Docker restarts. Written by the
/// installer and on every start, so servers installed earlier get it with an update.
pub fn install_boot_hook() -> Result<()> {
    let installed = std::path::Path::new("/usr/local/bin/tugport").exists();
    if !installed || !std::path::Path::new("/run/systemd/system").exists() || std::fs::read_to_string(DOCKER_DROPIN).is_ok_and(|c| c == DROPIN) {
        return Ok(());
    }
    let path = std::path::Path::new(DOCKER_DROPIN);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, DROPIN).with_context(|| format!("writing {DOCKER_DROPIN}"))?;
    run(Command::new("systemctl").arg("daemon-reload"))?;
    Ok(())
}

pub fn available() -> bool {
    Command::new("wg").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_addresses() {
        assert_eq!(mesh_ip(1), "10.99.0.1");
        assert_eq!(mesh_ip(2), "10.99.0.2");
        assert_eq!(mesh_ip(249), "10.99.0.249");
        assert_eq!(mesh_ip(250), "10.99.1.1");
        assert_eq!(mesh_ip(251), "10.99.1.2");
    }
}
