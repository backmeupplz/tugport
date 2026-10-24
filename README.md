<p align="center">
  <img src="website/logo.svg" width="96" alt="Tugport logo" />
</p>

<h3 align="center">Tugport</h3>
<p align="center">Deploy your apps to servers you own, from one small Rust binary.</p>

---

Tugport is a self-hosted deploy platform in the spirit of Coolify and Easypanel, built as a single Rust binary that runs next to Docker. Install it on a Linux server with systemd (x86_64 or arm64):

```sh
curl -fsSL https://tugport.com/install | sh
```

The installer sets up Docker if it is missing, starts Tugport as a system service and prints a one-time link for creating the owner account. Tugport is young (0.1.x), so expect rough edges.

## What it does

- **Deploys on push** from GitHub, Forgejo, Gitea, GitLab or any Git server with a deploy key, built with your Dockerfile, Railpack, Nixpacks or buildpacks. A new version gets traffic only after its health check passes, and rolling back takes one click.
- **Apps, Compose stacks and databases** (Postgres, MySQL, MariaDB, MongoDB, Redis) with domains, automatic HTTPS, environment, volumes and limits.
- **Logs and charts** for every server and service, with alerts to Telegram, email or a webhook.
- **AI agents as users:** an agent gets a scoped API key in one step, works through the API, the CLI or the MCP endpoint with the same permissions, and every action lands in the audit log.
- **Secrets that stay secret:** encrypted at rest, delivered only to the containers that use them, and masked everywhere else.
- **More servers and regions** joined by an encrypted WireGuard mesh, with cloud servers created at Hetzner or AWS, autoscaling, scale to zero and database replicas.
- **Backups** of databases and volumes to S3-compatible storage or a disk, with test restores.
- **Moving from Easypanel or Coolify** in one command while the apps keep running, with a rollback.

There is deliberately no app store, no one-click templates, no teams or organizations and no telemetry.

## Security notes

- Containers cannot reach the panel through the addresses configured on their own server, the server mesh or Tugport's internal endpoints. An address that lives on a NAT gateway instead, such as an EC2 Elastic IP or a home router's port forward, sends their traffic out and back in like any outside client's, so on such servers close the panel's port (8484) in the cloud firewall and use the panel through its domain.
- Privileged mode, the Docker socket, host networking, bind mounts and extra capabilities stay off unless an admin with the destroy grant turns them on.
- The master key is sealed to the server's TPM when it has one, otherwise kept in a root-only key file. Keep the recovery key the installer prints.

## Documentation

- [API reference](docs/API.md), including the MCP endpoint
- [Moving from Easypanel or Coolify](docs/MIGRATION.md)

## Repository

| Path | Contents |
|---|---|
| `src/` | The binary: panel API, proxy, deploy engine, builders, node agent, CLI and MCP endpoint |
| `web/` | The panel's web UI (Preact and Tailwind), embedded into release builds |
| `docs/` | API reference and migration guide |
| `dev/` | Docker-in-Docker boxes for building and testing on Linux (`dev/up.sh`) |
| `website/` | tugport.com, deployed to GitHub Pages on every push to `main` |

The share image `website/og.png` is rendered from `website/og.html` with headless Chrome:

```sh
cd website && "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --hide-scrollbars \
  --window-size=1200,630 --virtual-time-budget=10000 --screenshot="$PWD/og.png" "file://$PWD/og.html"
```

## License

MIT
