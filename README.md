<p align="center">
  <img src="website/logo.svg" width="96" alt="Tugport logo" />
</p>

<h3 align="center">Tugport</h3>
<p align="center">Deploy your apps to servers you own, from one small Rust binary.</p>

---

Tugport is a self-hosted alternative to Heroku and Vercel for Docker apps, and an alternative to Coolify, Dokploy and Easypanel: you push code, and it builds and runs your apps on servers you own, with HTTPS, databases and backups. It is a single Rust binary that runs next to Docker. Install it on a Linux server with systemd (x86_64 or arm64):

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
- **Automatic updates** of Tugport itself without dropping traffic, on by default and off with one setting.
- **Moving from Easypanel or Coolify** in one command while the apps keep running, with a rollback.

There is deliberately no app store, no one-click templates, no teams or organizations and no telemetry.

## How it compares

Coolify, Dokploy, Easypanel and CapRover do the same basic job as Tugport: they run your apps in Docker on your own servers behind HTTPS and give you a web panel. Tugport is the one that moves a running Easypanel or Coolify server over in one command and can roll that move back. It also documents autoscaling and scale to zero, joins servers through its own encrypted WireGuard network instead of Docker Swarm, and treats AI agents as users with their own roles, grants and audit trail. The panel, proxy, metrics and logs are one Rust program rather than a stack of services.

The alternatives are the better pick in plenty of cases. Coolify has hundreds of one-click templates, preview deployments and by far the largest community, and its self-hosted version is free with every feature. Dokploy has preview deployments on GitHub and a hosted plan if you would rather not run the panel yourself. CapRover and Dokku are small and have been around for years, and Kamal suits people who prefer to deploy over SSH from their own machine or CI with no panel at all. Tugport is young (0.1.x) and has a much smaller community, with no app store, templates, teams or preview deployments. Next to Heroku or Vercel, you run and pay for the servers yourself, and your apps run as long-lived containers, so there are no serverless functions or global edge network.

The [full comparison](docs/COMPARE.md) has a feature table, prices and sources for every claim.

## FAQ

### Which should I use for a few apps on a Hetzner server?

Coolify, Dokploy, Easypanel and Tugport all install on a plain Linux VPS such as a Hetzner Cloud server. Easypanel is free for 3 projects and paid per server after that, while the other three are free to self-host. If you want one-click templates for popular apps, Coolify is the easiest start. If you want zero-downtime deploys, scheduled backups with test restores and room to grow, Tugport can later create more Hetzner servers through your API token and spread replicas across them. The [Uptime Kuma guide](docs/UPTIME-KUMA.md) walks through a Tugport install on a Hetzner VPS step by step.

### How do I move my apps off Easypanel?

Install Tugport on the same server, which offers the move when it finds Easypanel, or run `tugport migrate easypanel --dry-run` to see the whole plan without changing anything. `tugport migrate easypanel` then moves projects, services, domains, certificates, environment, volumes and deploy webhooks while the apps keep running. Volumes stay where they are, certificates are imported rather than re-issued, and each database is down for a few seconds while it switches over. If anything goes wrong, `tugport migrate rollback` brings Easypanel back. Coolify works the same way with `tugport migrate coolify`. Some things, such as backup schedules, users and notification channels, are not moved; the [migration guide](docs/MIGRATION.md) lists them.

### Does it work with AI coding agents?

Yes, and that is one of the reasons it exists. The owner creates an agent user in one step and gets a key for it, along with ready-made snippets for Claude Code, Codex, the CLI and curl. The agent gets a role on just the projects it needs, and it can't delete anything or read secret values unless you give it those separate grants; keys can also expire and be limited to certain IP ranges. Agents work through the REST API, the `tugport` CLI or the MCP endpoint (`/mcp` over HTTP, or `tugport mcp` for clients that start a command), and every action lands in the audit log with the key behind it. [agents.md](website/agents.md) is a step-by-step guide an agent can follow to move a whole server to Tugport.

## Security notes

- Containers cannot reach the panel through the addresses configured on their own server, the server mesh or Tugport's internal endpoints. An address that lives on a NAT gateway instead, such as an EC2 Elastic IP or a home router's port forward, sends their traffic out and back in like any outside client's, so on such servers close the panel's port (8484) in the cloud firewall and use the panel through its domain.
- Privileged mode, the Docker socket, host networking, bind mounts and extra capabilities stay off unless an admin with the destroy grant turns them on.
- The master key is sealed to the server's TPM when it has one, otherwise kept in a root-only key file. Keep the recovery key the installer prints.

## Documentation

- [API reference](docs/API.md), including the MCP endpoint, at https://tugport.com/docs/api/
- [Moving from Easypanel or Coolify](docs/MIGRATION.md), at https://tugport.com/docs/migration/
- [Tugport compared with Coolify, Dokploy, Easypanel and CapRover](docs/COMPARE.md), at https://tugport.com/docs/compare/
- [Deploy Uptime Kuma on a Hetzner VPS with Tugport](docs/UPTIME-KUMA.md), tested step by step, at https://tugport.com/docs/uptime-kuma/
- [Moving a server to Tugport, for AI agents](website/agents.md), served at https://tugport.com/agents.md

## Repository

| Path | Contents |
|---|---|
| `src/` | The binary: panel API, proxy, deploy engine, builders, node agent, CLI and MCP endpoint |
| `web/` | The panel's web UI (Preact and Tailwind), embedded into release builds |
| `docs/` | API reference and migration guide |
| `dev/` | Docker-in-Docker boxes for building and testing on Linux (`dev/up.sh`) |
| `website/` | tugport.com, deployed to GitHub Pages on every push to `main`; its `docs/` pages are rendered from `docs/*.md` by `docs/build.mjs` (`cd docs && npm ci && node build.mjs`) |

The share image `website/og.png` is rendered from `website/og.html` with headless Chrome:

```sh
cd website && "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --hide-scrollbars \
  --window-size=1200,630 --virtual-time-budget=10000 --screenshot="$PWD/og.png" "file://$PWD/og.html"
```

Each docs page has its own card in `website/og/<slug>.png`; after adding or renaming a guide in `docs/guides.mjs`, run `node docs/og.mjs`.

## License

MIT
