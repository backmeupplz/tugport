<p align="center">
  <img src="website/logo.svg" width="96" alt="Tugport logo" />
</p>

<h3 align="center">Tugport</h3>
<p align="center">Deploy your apps to servers you own, from one small Rust binary.</p>

---

Tugport is a self-hosted deploy platform in the spirit of Coolify and Easypanel, built as a single Rust binary that runs next to Docker. Install it on a Linux server with `curl -fsSL https://tugport.com/install | sh`. It is young (0.1.x): everything below is built, and the installer, certificates, the GitHub App, cloud servers and Cloudflare Tunnel have been tested on real servers, so expect rough edges rather than missing pieces.

## What it does

- **One-command install** that offers to migrate an existing Easypanel or Coolify server while its apps keep running, with a rollback.
- **Projects and services:** apps from Git or images, Docker Compose stacks and databases, each with domains, automatic HTTPS, environment, volumes and limits.
- **Fast deploys on push** from GitHub or Forgejo, built with your Dockerfile, Railpack, Nixpacks or buildpacks, rolled out only after the health check passes and rolled back in one click.
- **Logs and charts:** live and searchable logs, plus CPU, memory, disk, network and GPU charts for every server and service.
- **Agents as users:** an agent user with a scoped API key in one step, per-project or all-project access, and an audit log of everything it did.
- **Secrets that stay secret:** encrypted at rest, delivered only to the containers that use them, and masked everywhere else.
- **More servers and regions** from the same panel, connected by an encrypted WireGuard mesh that containers cannot reach. Replicas spread over servers, and services move when a server goes away.
- **Cloud servers** created at Hetzner or AWS that join by themselves, and pools that grow and shrink with load.
- **Autoscaling and scale to zero:** replicas follow CPU or request rate, and idle sites sleep until the next request.
- **Database replicas** for Postgres, MySQL, MariaDB, MongoDB and Redis on other servers, with read-only URLs and promote.

There is deliberately no app store. The full plan is in [ROADMAP.md](ROADMAP.md).

## Repository

| Path | Contents |
|---|---|
| `src/` | The binary: panel API, proxy, deploy engine, builders, node agent, CLI and MCP endpoint |
| `web/` | The panel's web UI (Preact and Tailwind), embedded into release builds |
| `docs/` | [API reference](docs/API.md) and [moving from Easypanel or Coolify](docs/MIGRATION.md) |
| `dev/` | Docker-in-Docker boxes for building and testing on Linux |
| `website/` | tugport.com, deployed to GitHub Pages on every push to `main` |

The share image `website/og.png` is rendered from `website/og.html` with headless Chrome:

```sh
cd website && "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --hide-scrollbars \
  --window-size=1200,630 --virtual-time-budget=10000 --screenshot="$PWD/og.png" "file://$PWD/og.html"
```

## License

MIT
