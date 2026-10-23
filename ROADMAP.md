# Tugport roadmap

Tugport is a self-hosted deploy platform that ships as one Rust binary next to Docker. The goal is a small, fast and secure tool. It has no app store, it has defaults you rarely need to change, and it gives AI agents first-class access with exactly the permissions you choose.

Even on a single server, the panel drives Docker through the same node interface that remote servers use. Adding servers in milestone 2 therefore needs no rewrite.

## Milestone 1: one server, complete

**Status:** built and tested on development servers. On AWS (2026-10-23), the one-line installer set up a fresh Ubuntu 24.04 server and Let's Encrypt issued a real certificate. On Hetzner, the GitHub App flow built and deployed a push and posted the commit status. Cloudflare Tunnel served sites through Cloudflare and removed its tunnel and DNS records again on disconnect.

### Install and migrate
- **One command:** `curl -fsSL https://tugport.com/install | sh`.
  - Installs Docker if it is missing.
  - Downloads the checksum-verified binary and sets up the system service.
  - Prints a one-time login link and a recovery key.
- **Migration:** the installer detects an existing Easypanel or Coolify install and offers to migrate it.
  - **What moves:** projects, services, domains, certificates, environment, volumes and deploy webhooks. Old webhook URLs keep working.
  - **No downtime:** each service moves with a blue/green switch.
  - **Rollback:** the old panel is stopped only at the end and can be started again.

### Projects and services
- **Projects** hold services and shared variables. Services in the same project reach each other by name on a private network.
- **Apps** come from:
  - a Git repository (GitHub, Forgejo/Gitea, GitLab or any Git URL with a deploy key);
  - a Docker image (with registry credentials);
  - an inline Dockerfile.
- **Docker Compose stacks** come from a repository or pasted YAML.
  - Every container shows up with its own logs, charts and domains.
  - `build:` sections use the same builder.
- **Databases:** Postgres, MySQL/MariaDB, MongoDB and Redis.
  - **Image and version** are editable, pre-filled with the newest stable release and pinned exactly.
  - **Credentials** are generated, and a connection string is offered to other services.
  - **Major upgrades** are manual and take a backup first.
- **Per-service settings:**
  - domains with automatic HTTPS;
  - environment and secrets;
  - volume, bind and file mounts;
  - published TCP/UDP ports;
  - CPU and memory limits, replicas, command and health check;
  - GPU access;
  - capabilities and groups.
- **Console:** a shell into any service from the browser, or `tugport exec` from the CLI.

### Builds
- **Builders:** Dockerfile through BuildKit, Railpack (the default when there is no Dockerfile), Nixpacks, and Heroku/Paketo buildpacks.
- **Fast by default:**
  - persistent per-service BuildKit cache;
  - cache mounts for npm, pnpm, yarn, bun, pip, uv, poetry, cargo, Go, Maven and Gradle;
  - dependencies installed from lockfiles before the source is copied;
  - incremental `git fetch` instead of fresh clones;
  - parallel builds;
  - timing for every build step, so slow steps are visible.
- **Resource caps:** builds have CPU and memory caps, so a build cannot starve running services.

### Domains and networking
- **Built-in proxy** with automatic Let's Encrypt certificates and retries.
  - Supports HTTP/2, WebSockets and gRPC.
  - Timeouts are configurable.
  - Client IPs are taken only from trusted proxies.
- **Routing extras:** redirects, basic auth, forward auth, path rewrite and strip, and custom error and maintenance pages.
- **Cloudflare Tunnel out of the box:** connect a Cloudflare token and choose, per domain, whether it goes through the tunnel. Tugport runs `cloudflared` and creates the tunnel routes and DNS records itself.
- **Panel access:** the panel can listen on a private address only, while webhooks still arrive on a public path.

### Deploys
- **Triggers:**
  - auto-deploy on push from GitHub through a GitHub App, where webhooks are set up automatically;
  - auto-deploy on push from Forgejo/Gitea through a token and webhook;
  - a deploy webhook URL for CI and image pushes.
- **Watch paths:** a push to a monorepo redeploys only the services whose folders changed.
- **When a new deploy starts while another is running,** the running one is cancelled by default. This can be set to queue instead, globally or per service.
- **Isolation:** a failed or cancelled build never affects the panel or other deploys.
- **Zero-downtime rollouts:**
  - The new version gets traffic only after its health check passes.
  - A failed health check rolls back automatically.
  - One click rolls back to any of the last few images.
- **Status reporting:** deploy status is posted back on the commit and sent as a notification.

### Logs
- **Live tail** of every replica of a service, with search and time ranges.
- **History** survives restarts and redeploys, and each deploy keeps its build log.
- **Container events:** exits, out-of-memory kills and failed health checks.
- **Tugport's own log** is viewable in the panel with level filters, and through `tugport logs --self`.
- **Automatic cleanup** is off by default. It deletes the oldest logs once they pass a configurable size or age.

### Server charts and housekeeping
- **Machine charts:** CPU (total and per core), load, memory and swap, disk space and I/O, network, temperatures.
- **GPU charts:** utilization, VRAM, temperature and power.
- **Per-service charts:** CPU, memory, network and restarts. History ranges from one hour to 30 days, stored in Tugport's own database.
- **Alerts:** sent to Telegram, email or a webhook.
  - High CPU, memory, disk or GPU.
  - Crash loops and out-of-memory kills.
  - Failed deploys, certificate renewals and backups.
- **Automatic Docker cleanup** is on by default.
  - Removes unused images and caps the build cache.
  - Keeps the images of running services and the last few per service for rollback.
  - Never touches volumes.

### Backups and updates
- **Database backups:**
  - scheduled, to S3-compatible storage or local disk;
  - with retention, one-click restore and test restores.
- **Volume backups** with restore.
- **Tugport's own state** is one database file plus its key, so full recovery means reinstalling and restoring that file.
- **Image auto-update** for services that run a Docker image. It is off by default.
  - The check interval is configurable.
  - It can follow a tag, or newer versions within the same major.
  - Updates roll out with health checks and roll back on failure.
- **Self-update of Tugport** without dropping traffic.

### Access, agents and secrets
- **Two kinds of user:**
  - Humans sign in with a password and TOTP.
  - Agents have no password and no UI, only API keys.
- **New agent in one step:** choose a name, projects and a role. The key is shown once, with copy-paste blocks for environment variables, the CLI and MCP clients.
- **Project access** is either a list of projects or **all projects**, which includes projects created later.
- **Roles:**
  - viewer: status, charts, logs;
  - deployer: also deploy, restart, rollback;
  - operator: also environment, domains, mounts, new services;
  - admin: also delete, members, keys.
- **Destructive actions** need a separate grant that no preset role includes.
- **Keys** can expire and can be limited to IP ranges. Each shows when and where it was last used, and can be revoked instantly or rotated.
- **Audit log:** every action records the actor, whether human, agent or key, and shows up on the service timeline.
- **Agent-friendly API:**
  - an OpenAPI spec;
  - `deploy --wait`, which returns the outcome and log tail;
  - an event stream;
  - the CLI and MCP endpoint, which apply exactly the same permissions as the key behind them.
- **Secrets and environment are always encrypted:**
  - Every value is stored encrypted, not only those marked secret.
  - The master key is sealed to the server's TPM when one exists. Otherwise it lives in a root-only key file kept outside the database and backups, with a recovery key shown once at install.
  - An optional locked mode needs a passphrase after every reboot.
- **Secrets reach only the containers that use them:**
  - They are delivered over encrypted links to the server that runs the container.
  - Inside the container they are written to memory-only storage.
  - Tugport's init shim loads them into the app's environment, so they never appear on disk, in `docker inspect` or in image layers. Builds receive them as BuildKit secrets, never as build arguments.
- **Secret values are masked** in logs, API, CLI and MCP output, notifications and error messages. Agents can write secrets but cannot read them without an explicit grant.
- **A compromised service cannot steal other services' secrets:**
  - Each container only ever sees its own secrets, delivered as a memory-only file mounted into that container alone.
  - Services cannot mount the Docker socket, Tugport's data or another service's secrets unless an admin explicitly allows it.
  - Containers cannot reach the panel's port through the addresses configured on their own server, nor the server mesh or Tugport's internal endpoints, and there is no metadata endpoint to query. An address that lives on a NAT gateway instead, such as an EC2 Elastic IP or a home router's port forward, sends their traffic out and back in like any outside client's, so on such servers close the panel's port in the cloud firewall and use the panel through its domain. A container on another server reaches the panel's port like any other machine on the network that port listens on, and the panel domain answers every container the way it answers anyone on the internet, always behind the same sign-in or webhook token. A service on the panel's own server, such as a self-hosted Git server, therefore sends deploy webhooks through the panel domain or the webhook-only port. Services that an admin puts on the host network are not containers in this sense.
  - Projects sit on separate networks, so a service cannot reach another project's databases.
  - Containers start with `no-new-privileges` and are never privileged unless an admin allows it.
- **No telemetry:** Tugport sends none, and its own outbound calls never carry environment values.

### Settings
- **About:** the version, the commit and a link to the source code (github.com/backmeupplz/tugport).
- **Updates:** the update channel and an option to check now.

## Milestone 2: more servers

**Status:** built and tested with two development servers: joining, the mesh, placement, routing between servers, central logs and metrics, rescheduling when a server goes away, and restoring it when it returns.

- **Joining:** a server joins with one command shown in the panel: `curl -fsSL https://tugport.com/install | sh -s -- --join <panel> --token <token>`.
- **Mesh:** servers form an encrypted WireGuard mesh that Tugport sets up itself, and services reach each other by name across servers.
- **Replicas and placement:**
  - Replicas spread across servers, and every server's proxy balances over the healthy ones.
  - Services can be pinned to a server, a label or a region.
- **Images:** each image is built once and sent, over the encrypted mesh, only to the servers that need it.
- **Ingress:** by default only the panel faces the public internet; other servers hold no certificates or sign-in settings and serve only forwarded requests. Point DNS at another server and you mark it as an ingress.
- **One panel for everything:** charts, logs, the console and alerts cover every server.
- **Resilience:** apps keep running if the panel's server goes down. Changes pause until it is back.

## Milestone 3: cloud and data

**Status:** built. Scale to zero, replica autoscaling and database replicas (all five engines, with promote) are tested with two development servers. On AWS (2026-10-23), a panel created an EC2 machine that joined over the mesh within minutes, served a two-replica app over HTTPS from both servers, and was terminated again when removed. A Hetzner server pool grew by a real machine that joined in about three minutes, and shrank again.
- **Cloud servers:** connect an AWS or Hetzner account to create servers in any region. They join the mesh automatically.
- **Autoscaling:** add and remove replicas by CPU or request rate, then add and remove whole servers.
- **Scale to zero:** idle services stop and start again on the first request.
- **Database replicas:** Postgres streaming read replicas, MongoDB replica sets, MySQL replicas and Redis replicas.
  - Replicas run on other servers or in other regions.
  - Each database gets read-only connection strings and a promote button.

## Not planned
- App store and one-click templates
- WordPress and dev-box services
- One-click database admin tools
- Teams and organizations
- Secret-manager integrations
- Log drains
- Traffic analytics
- White-labeling
- Deploying from an uploaded archive
- CI runners
- OS patching
