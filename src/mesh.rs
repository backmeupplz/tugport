//! The WireGuard mesh between servers: one interface (`tugport0`), one UDP port, every
//! server a peer of every other. Addresses live in 10.99.0.0/16; the panel's server is .1.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use crate::proc::{run_sync, succeeds};
use std::process::Command;

pub const IFACE: &str = "tugport0";
pub const PORT: u16 = 51820;
pub const PANEL_IP: &str = "10.99.0.1";
pub const SUBNET: &str = "10.99.0.0/16";

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

/// New private key and its public key.
pub fn keypair() -> Result<(String, String)> {
    let private = run_sync(Command::new("wg").arg("genkey"))?;
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
    let exists = succeeds(Command::new("ip").args(["link", "show", IFACE]));
    if !exists {
        run_sync(Command::new("ip").args(["link", "add", IFACE, "type", "wireguard"]))?;
    }
    run_sync(Command::new("wg").args(["set", IFACE, "listen-port", &PORT.to_string(), "private-key"]).arg(private_key_file))?;
    let addrs = run_sync(Command::new("ip").args(["-o", "addr", "show", "dev", IFACE]))?;
    if !addrs.contains(&format!("{own_ip}/16")) {
        run_sync(Command::new("ip").args(["addr", "add", &format!("{own_ip}/16"), "dev", IFACE]))?;
    }
    // VXLAN (Docker overlay) runs inside WireGuard, so leave room for both headers.
    run_sync(Command::new("ip").args(["link", "set", "mtu", "1420", "up", "dev", IFACE]))?;
    Ok(())
}

/// Make the interface's peers exactly `peers` (adds, updates and removes).
pub fn set_peers(peers: &[Peer]) -> Result<()> {
    let current = run_sync(Command::new("wg").args(["show", IFACE, "peers"])).unwrap_or_default();
    for key in current.lines().filter(|k| !k.is_empty()) {
        if !peers.iter().any(|p| p.public_key == key) {
            run_sync(Command::new("wg").args(["set", IFACE, "peer", key, "remove"]))?;
        }
    }
    for p in peers {
        let mut cmd = Command::new("wg");
        cmd.args(["set", IFACE, "peer", &p.public_key, "allowed-ips", &format!("{}/32", p.mesh_ip), "persistent-keepalive", "25"]);
        if let Some(ep) = &p.endpoint {
            cmd.args(["endpoint", ep]);
        }
        run_sync(&mut cmd)?;
    }
    Ok(())
}

/// Containers must never open connections into the mesh: from there they could reach other
/// servers' Docker, agents, proxies (as a trusted hop) and the image registry. Only traffic
/// arriving over WireGuard itself, or from this host, may start a mesh connection.
pub fn guard() -> Result<()> {
    const CHAIN: &str = "TUGPORT-MESH";
    (|| {
        new_chain("iptables", CHAIN)?;
        // In FORWARD the rule would end up below the ACCEPT rules Docker inserts when it starts.
        let forward = if docker_user_chain().is_ok() { "DOCKER-USER" } else { "FORWARD" };
        ensure("iptables", "-A", CHAIN, &["-i", IFACE, "-j", "RETURN"])?;
        ensure("iptables", "-A", CHAIN, &["-i", "lo", "-j", "RETURN"])?;
        ensure("iptables", "-A", CHAIN, &["-m", "conntrack", "--ctstate", "NEW", "-j", "DROP"])?;
        // Published ports are DNAT-ed before filtering, so match the original destination.
        ensure("iptables", "-I", forward, &["-m", "conntrack", "--ctorigdst", SUBNET, "-j", CHAIN])?;
        ensure("iptables", "-I", "INPUT", &["-d", SUBNET, "-j", CHAIN])
    })()
    .context("blocking containers from the mesh")
}

/// Swarm's ports carry overlay traffic without authentication: VXLAN (4789/udp), gossip
/// (7946) and management (2377). Tugport's swarm runs over the mesh, so they answer only there
/// and to this host. `node_addr` is the swarm address Docker reports; a swarm that was here
/// before Tugport (an old panel's) and talks over other addresses is left alone.
pub fn guard_swarm(node_addr: Option<&str>) -> Result<()> {
    const CHAIN: &str = "TUGPORT-SWARM";
    if !node_addr.is_some_and(|a| a.starts_with("10.99.")) {
        return Ok(());
    }
    // The mesh is IPv4 only, so over IPv6 these ports answer nobody but this host.
    for (bin, mesh) in [("iptables", true), ("ip6tables", false)] {
        if !has(bin, &["-n", "-L", "INPUT"]) {
            continue;
        }
        (|| {
            new_chain(bin, CHAIN)?;
            if mesh {
                ensure(bin, "-A", CHAIN, &["-i", IFACE, "-j", "RETURN"])?;
            }
            ensure(bin, "-A", CHAIN, &["-i", "lo", "-j", "RETURN"])?;
            ensure(bin, "-A", CHAIN, &["-j", "DROP"])?;
            ensure(bin, "-I", "INPUT", &["-p", "udp", "-m", "multiport", "--dports", "4789,7946", "-j", CHAIN])?;
            ensure(bin, "-I", "INPUT", &["-p", "tcp", "-m", "multiport", "--dports", "2377,7946", "-j", CHAIN])
        })()
        .context("guarding swarm's ports")?;
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
    new_chain("iptables", CHAIN).and_then(|_| ensure("iptables", "-I", "FORWARD", &["-j", CHAIN])).context("creating DOCKER-USER")
}

/// `iptables` or `ip6tables` with `args`. At boot Docker, firewalls and Tugport may all hold
/// the xtables lock for a moment, so wait for it, but bounded: a hung holder would otherwise
/// keep Docker from starting (it runs the guards first).
fn xtables(bin: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(["-w", "10"]).args(args);
    cmd
}

fn has(bin: &str, args: &[&str]) -> bool {
    succeeds(&mut xtables(bin, args))
}

fn run(bin: &str, args: &[&str]) -> Result<String> {
    run_sync(&mut xtables(bin, args))
}

/// Create `chain` unless it exists.
fn new_chain(bin: &str, chain: &str) -> Result<()> {
    if !has(bin, &["-n", "-L", chain]) {
        run(bin, &["-N", chain])?;
    }
    Ok(())
}

/// Add `rule` to `chain` (`op` is `-A` or `-I`) unless it is there already.
fn ensure(bin: &str, op: &str, chain: &str, rule: &[&str]) -> Result<()> {
    if !has(bin, &[&["-C", chain], rule].concat()) {
        run(bin, &[&[op, chain], rule].concat())?;
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
    run_sync(Command::new("systemctl").arg("daemon-reload"))?;
    Ok(())
}

/// Containers must not reach the panel through the host's own addresses (its LAN or public
/// IP, a bridge gateway) either. Such connections arrive on INPUT from a container bridge,
/// while the proxy's come over loopback and everyone else's over a real interface. Agents
/// listen on the mesh address only, which `guard` already covers.
///
/// `host_ifaces` (from `interfaces`) must be read before `docker_bridges` (the interfaces of
/// Docker's bridge networks), so a network created in between is never taken for a host
/// bridge. Networks created later are covered anyway: Docker names them `br-<id>`, and `br-+`
/// matches them.
pub fn guard_panel(ports: &[u16], host_ifaces: &[String], docker_bridges: Option<&[String]>) -> Result<()> {
    // Without Docker's list a network named `br-something` would pass for a host bridge, so
    // keep the rules in place, or start with the default bridges and `br-+`.
    let rules = match docker_bridges {
        Some(bridges) => panel_rules(ports, bridges, host_ifaces),
        None => panel_rules(ports, &[], &[]),
    };
    let keep = docker_bridges.is_none();
    let v4 = guard_panel_with("iptables", &rules, keep);
    // Only containers on IPv6-enabled networks reach a panel listening on `[::]`.
    let v6 = if has("ip6tables", &["-n", "-L", "INPUT"]) {
        guard_panel_with("ip6tables", &rules, keep)
    } else {
        Ok(())
    };
    v4.and(v6)
}

/// This server's network interfaces.
pub fn interfaces() -> Vec<String> {
    std::fs::read_dir("/sys/class/net").map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default()
}

const PANEL_CHAIN: &str = "TUGPORT-PANEL";

/// Docker's own name for a bridge network's interface: `br-` and 12 hex digits of its id.
fn docker_named(iface: &str) -> bool {
    iface.strip_prefix("br-").is_some_and(|id| id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The chain's rules. Host bridges that happen to be called `br-something` (netplan's
/// `br-wan`, OpenStack's `br-ex`) carry real clients and are let through first; bridges that
/// Docker was told to name otherwise are added to the defaults. Containers get a reset rather
/// than silence, so a webhook aimed at the wrong address fails at once.
fn panel_rules(ports: &[u16], docker_bridges: &[String], host_ifaces: &[String]) -> Vec<Vec<String>> {
    if ports.is_empty() {
        return vec![];
    }
    let mut rules: Vec<Vec<String>> = host_ifaces
        .iter()
        .filter(|i| i.starts_with("br-") && !docker_named(i) && !docker_bridges.contains(i))
        .map(|i| vec!["-i".into(), i.clone(), "-j".into(), "RETURN".into()])
        .collect();
    let mut bridges: Vec<&str> = vec!["docker0", "docker_gwbridge", "br-+"];
    bridges.extend(docker_bridges.iter().map(String::as_str).filter(|b| !b.starts_with("br-") && !bridges.contains(b)).collect::<Vec<_>>());
    for b in bridges {
        for port in ports {
            rules.push(["-i", b, "-p", "tcp", "--dport", &port.to_string(), "-m", "conntrack", "--ctstate", "NEW", "-j", "REJECT", "--reject-with", "tcp-reset"]
                .map(String::from)
                .to_vec());
        }
    }
    rules
}

/// `keep`: leave a chain that has rules alone, only making sure INPUT still jumps to it.
fn guard_panel_with(bin: &str, rules: &[Vec<String>], keep: bool) -> Result<()> {
    let rules: Vec<Vec<&str>> = rules.iter().map(|r| r.iter().map(String::as_str).collect()).collect();
    (|| {
        new_chain(bin, PANEL_CHAIN)?;
        // Ports and bridges change, so rebuild the chain whenever it holds anything else.
        let present = run(bin, &["-S", PANEL_CHAIN])?.lines().filter(|l| l.starts_with("-A ")).count();
        if !(keep && present > 0) && (present != rules.len() || !rules.iter().all(|r| has(bin, &[&["-C", PANEL_CHAIN], &r[..]].concat()))) {
            run(bin, &["-F", PANEL_CHAIN])?;
            for r in &rules {
                run(bin, &[&["-A", PANEL_CHAIN], &r[..]].concat())?;
            }
        }
        ensure(bin, "-I", "INPUT", &["-p", "tcp", "-j", PANEL_CHAIN])
    })()
    .context("blocking containers from the panel")
}

pub fn available() -> bool {
    succeeds(Command::new("wg").arg("--version"))
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

    #[test]
    fn panel_rules_cover_container_bridges_only() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(super::panel_rules(&[], &s(&["docker0"]), &s(&["eth0"])).is_empty());
        assert_eq!(super::panel_rules(&[8484, 9000], &[], &[]).len(), 6);
        let rules: Vec<String> = super::panel_rules(
            &[8484],
            &s(&["docker0", "br-0123456789ab", "br-custom", "tp-custom"]),
            &s(&["lo", "eth0", "docker0", "br-0123456789ab", "br-ffffffffffff", "br-custom", "br-ex", "br-wan"]),
        )
        .iter()
        .map(|r| r.join(" "))
        .collect();
        let drop = |i: &str| format!("-i {i} -p tcp --dport 8484 -m conntrack --ctstate NEW -j REJECT --reject-with tcp-reset");
        assert_eq!(
            rules,
            vec![
                "-i br-ex -j RETURN".to_string(),
                "-i br-wan -j RETURN".to_string(),
                drop("docker0"),
                drop("docker_gwbridge"),
                drop("br-+"),
                drop("tp-custom"),
            ]
        );
    }
}
