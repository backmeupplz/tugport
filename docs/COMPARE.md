# Tugport compared with Coolify, Dokploy, Easypanel and CapRover

These are the self-hosted deploy panels people most often weigh against each other. All five run your apps in Docker on servers you own, put HTTPS in front of them and give you a web panel. This page shows where they differ, using each project's own docs, pricing page and GitHub repository, checked on October 27, 2026. Projects change quickly, so the [sources](#sources) are listed at the end. If something here is out of date, [open an issue](https://github.com/backmeupplz/tugport/issues) and it will be fixed.

Tugport is the only one of the five that moves a running Easypanel or Coolify server over in one command, and the only one that documents autoscaling and scale to zero.

## At a glance

| Feature | Tugport | Coolify | Dokploy | Easypanel | CapRover |
|---|---|---|---|---|---|
| License | MIT | Apache-2.0 | Apache-2.0, enterprise folder excepted | Proprietary | Apache-2.0, with an appendix |
| Written in | Rust, one binary | PHP (Laravel) | TypeScript | Not published | TypeScript |
| GitHub stars | 4 | 64,982 | 38,063 | Closed source | 15,193 |
| Public since | 2026 | 2021 | 2024 | – | 2017 |
| Self-hosted | Free | Free | Free; enterprise paid | 3 projects free, then from $14.90/server | Free; Pro paid |
| Hosted panel | No | From $5 a month | From $4.50/server | No | No |
| Many servers | Built-in network | SSH servers | Remote servers, Swarm | Swarm | Swarm |
| Zero-downtime deploys | Yes | Rolling updates | Yes | Optional | Not documented |
| Autoscaling, scale to zero | Yes | Not documented | Not documented | Not documented | Not documented |
| Docker Compose | Yes | Yes | Yes | Yes | Partly |
| One-click templates | None | 360+ | Yes | Yes | Dozens |
| Database backups | S3 or disk, tested | To S3 | To S3 | Several targets | None built in |
| Preview deployments | No | Yes | Yes, on GitHub | Not documented | Not documented |
| AI agents | Agent users, MCP | MCP server | AI assistant | Remote MCP | API and CLI |
| Moving from another panel | One command, with rollback | No importer | No importer | No importer | No importer |

### Behind the table

- **Licenses.** Dokploy is Apache-2.0 except a source-available folder of enterprise features, and CapRover's Apache-2.0 license has an appendix about its paid features.
- **Prices.** Easypanel is free for 3 projects, then $14.90 to $37.90 per server a month ($10.90 to $29.90 billed yearly). CapRover's paid Pro plan adds 2FA, build notifications and login alerts. Coolify Cloud costs $5 a month for 2 servers and $3 per extra server; Dokploy Cloud starts at $4.50 per server a month. Coolify's self-hosted version is free with every feature.
- **More than one server.** Tugport servers join an encrypted WireGuard network with one command, or Tugport creates them at Hetzner or AWS; replicas spread across servers and move when one goes away. Coolify runs standalone servers over SSH, behind a load balancer you provide, and its Swarm support is experimental. Dokploy uses remote servers or Docker Swarm nodes, Easypanel Docker Swarm worker nodes, and CapRover Docker Swarm nodes.
- **Zero-downtime deploys.** Tugport moves traffic once the new version passes its health check. Coolify rolls updates unless a container has a fixed name, Dokploy needs a health check route, and in Easypanel it's a per-service option.
- **Scale to zero and autoscaling.** In Tugport, idle sites sleep until a request wakes them, and replicas follow CPU or request rate. The other four don't document either.
- **Docker Compose.** CapRover supports an experimental subset.
- **Backups.** Tugport schedules them to S3-compatible storage or a disk, with test restores, and backs up volumes too. Coolify and Dokploy back up to S3, and Easypanel to several kinds of destinations.
- **AI agents.** In Tugport, agents are users with roles and keys: deleting and reading secrets need separate grants, there's an MCP endpoint, and every action lands in an audit log. Coolify has a built-in MCP server used with team API tokens. Dokploy has a built-in AI assistant that uses your own model's API key. Easypanel has a remote MCP server that acts with a chosen user's permissions, and CapRover has an API and a CLI.
- **Moving over.** `tugport migrate easypanel` and `tugport migrate coolify` move a running server, and `tugport migrate rollback` undoes it.

Dokku and Kamal come up in the same searches. Both are command-line tools: Dokku's web panel is part of the paid Dokku Pro, and Kamal deploys from your own machine or CI over SSH, with no panel.

## Why Tugport

- **Moving over.** `tugport migrate easypanel` and `tugport migrate coolify` take a running server's projects, services, domains, certificates, environment, volumes and webhooks while the apps keep running, and `tugport migrate rollback` brings the old panel back. None of the other panels document an importer. The [migration guide](https://tugport.com/docs/migration/) has the details; its author's own production server moved 14 projects and 57 services this way.
- **Servers come and go.** Replicas spread across servers and move when one disappears, apps can scale with load or sleep until a request comes in, and Tugport can create the servers itself.
- **AI agents get their own users.** An agent gets a role on the projects it needs, and it can't delete anything or read a secret unless you give it those separate grants. Every action lands in the audit log with the key behind it.
- **One binary.** The panel, the reverse proxy, the metrics and the logs are a single Rust program next to Docker, not a stack of services that need looking after themselves.

## Which one fits

- **You already run Easypanel or Coolify and want zero-downtime deploys, more servers and agent access without rebuilding anything:** try Tugport. `tugport migrate easypanel --dry-run` (or `coolify`) prints the whole plan and changes nothing.
- **You want one-click apps and a big community:** Coolify. It's free with every feature, and it has the largest catalog.
- **You want Docker Swarm across several servers, with a hosted option:** Dokploy.
- **You want something small and proven on a single server:** CapRover or Dokku.

## Sources

Checked on October 27, 2026.

- Tugport: [tugport.com](https://tugport.com/), [API reference](https://tugport.com/docs/api/), [migration guide](https://tugport.com/docs/migration/), [GitHub](https://github.com/backmeupplz/tugport)
- Coolify: [docs](https://coolify.io/docs), [pricing](https://coolify.io/pricing), [rolling updates](https://coolify.io/docs/applications/deployments/rolling-updates), [GitHub](https://github.com/coollabsio/coolify)
- Dokploy: [docs](https://docs.dokploy.com), [zero-downtime deployments](https://docs.dokploy.com/docs/core/applications/zero-downtime), [pricing](https://dokploy.com/pricing), [license](https://github.com/Dokploy/dokploy/blob/canary/LICENSE.MD), [GitHub](https://github.com/Dokploy/dokploy)
- Easypanel: [docs](https://easypanel.io/docs), [MCP server](https://easypanel.io/docs/mcp), [pricing](https://easypanel.io/pricing)
- CapRover: [docs](https://caprover.com/docs/get-started.html), [Pro features](https://caprover.com/docs/troubleshooting-pro.html), [license](https://github.com/caprover/caprover/blob/master/LICENSE), [GitHub](https://github.com/caprover/caprover)
- Dokku and Kamal: [dokku.com](https://dokku.com/), [Dokku Pro](https://pro.dokku.com/), [kamal-deploy.org](https://kamal-deploy.org/)
