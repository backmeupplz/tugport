# Tugport compared with Coolify, Dokploy, Easypanel and CapRover

These are the self-hosted deploy panels people most often weigh against each other. All five run your apps in Docker on servers you own, put HTTPS in front of them and give you a web panel. This page shows where they differ, using each project's own docs, pricing page and GitHub repository, checked on October 27, 2026. Projects change quickly, so the [sources](#sources) are listed at the end. If something here is out of date, [open an issue](https://github.com/backmeupplz/tugport/issues) and it will be fixed.

Tugport is by far the youngest and smallest of the five, and the table says plainly where it's behind.

## At a glance

| Feature | Tugport | Coolify | Dokploy | Easypanel | CapRover |
|---|---|---|---|---|---|
| License | MIT | Apache-2.0 | Apache-2.0, except a source-available folder of enterprise features | Proprietary | Apache-2.0, with an appendix about its paid features |
| Written in | Rust, one binary | PHP (Laravel) | TypeScript | Not published | TypeScript |
| GitHub stars | 4 | 64,982 | 38,063 | The panel's source isn't public | 15,193 |
| Public since | October 2026 | 2021 | 2024 | – | 2017 |
| Self-hosted price | Free | Free, every feature | Free; enterprise features are paid | Free for 3 projects, then $14.90 to $37.90 per server a month ($10.90 to $29.90 billed yearly) | Free; a paid Pro plan adds 2FA, build notifications and login alerts |
| Hosted panel | No | Coolify Cloud: $5 a month for 2 servers, $3 per extra server | Dokploy Cloud: from $4.50 per server a month | No | No |
| More than one server | Servers join an encrypted WireGuard network with one command, or Tugport creates them at Hetzner or AWS; replicas spread across servers and move when one goes away | Standalone servers over SSH, behind a load balancer you provide; Swarm support is experimental | Remote servers, or Docker Swarm nodes | Docker Swarm worker nodes | Docker Swarm nodes |
| Zero-downtime deploys | Yes: traffic moves after the new version passes its health check | Rolling updates, unless a container has a fixed name | Yes, with a health check route | Yes, a per-service option | Not stated in its docs |
| Scale to zero and autoscaling | Yes: idle sites sleep until a request wakes them, and replicas follow CPU or request rate | Not documented | Not documented | Not documented | Not documented |
| Docker Compose | Yes | Yes | Yes | Yes | Partly: an experimental subset |
| One-click app templates | None | 360+ | Yes | Yes | Yes, dozens |
| Database backups | Scheduled, to S3-compatible storage or a disk, with test restores; volumes too | Scheduled, to S3 | Scheduled, to S3 | Scheduled, to several destinations | No built-in database backups |
| Preview deployments | No | Yes | Yes, on GitHub | Not documented | Not documented |
| AI agents | Agents are users with roles and keys; deleting and reading secrets need separate grants; MCP endpoint; audit log | Built-in MCP server, used with team API tokens | Built-in AI assistant that uses your own model's API key | Remote MCP server that acts with a chosen user's permissions | API and CLI |
| Moving from another panel | `tugport migrate easypanel` and `tugport migrate coolify`, with `rollback` | No importer documented | No importer documented | No importer documented | No importer documented |

Dokku and Kamal come up in the same searches. Both are command-line tools: Dokku's web panel is part of the paid Dokku Pro, and Kamal deploys from your own machine or CI over SSH, with no panel.

## Where Tugport is ahead

- **Moving over.** `tugport migrate easypanel` and `tugport migrate coolify` take a running server's projects, services, domains, certificates, environment, volumes and webhooks while the apps keep running, and `tugport migrate rollback` brings the old panel back. None of the other panels document an importer. The [migration guide](https://tugport.com/docs/migration/) has the details; its author's own production server moved 14 projects and 57 services this way.
- **Servers come and go.** Replicas spread across servers and move when one disappears, apps can scale with load or sleep until a request comes in, and Tugport can create the servers itself.
- **AI agents get their own users.** An agent gets a role on the projects it needs, and it can't delete anything or read a secret unless you give it those separate grants. Every action lands in the audit log with the key behind it.
- **One binary.** The panel, the reverse proxy, the metrics and the logs are a single Rust program next to Docker, not a stack of services that need looking after themselves.

## Where Tugport is behind

- **It's new.** The first release came out in October 2026. It has a handful of stars, its bug reports so far come mostly from its author's own servers, and there is no company behind it. Coolify, Dokploy and CapRover have years of releases and large communities.
- **No one-click templates.** To run Plausible, Ghost or n8n, you deploy their image or Compose file yourself. Coolify alone lists more than 360 ready-made services.
- **No preview deployments** for pull requests yet.
- **No hosted panel.** Coolify and Dokploy will run the panel for you; with Tugport you always run it yourself.

## Which one fits

- **You want one-click apps and a big community:** Coolify. It's free with every feature, and it has the largest catalog.
- **You want Docker Swarm across several servers, with a hosted option:** Dokploy.
- **You already run Easypanel or Coolify and want zero-downtime deploys, more servers and agent access without rebuilding anything:** try Tugport. `tugport migrate easypanel --dry-run` (or `coolify`) prints the whole plan and changes nothing.
- **You want something small and proven on a single server:** CapRover or Dokku.

## Sources

Checked on October 27, 2026.

- Tugport: [tugport.com](https://tugport.com/), [API reference](https://tugport.com/docs/api/), [migration guide](https://tugport.com/docs/migration/), [GitHub](https://github.com/backmeupplz/tugport)
- Coolify: [docs](https://coolify.io/docs), [pricing](https://coolify.io/pricing), [rolling updates](https://coolify.io/docs/applications/deployments/rolling-updates), [GitHub](https://github.com/coollabsio/coolify)
- Dokploy: [docs](https://docs.dokploy.com), [zero-downtime deployments](https://docs.dokploy.com/docs/core/applications/zero-downtime), [pricing](https://dokploy.com/pricing), [license](https://github.com/Dokploy/dokploy/blob/canary/LICENSE.MD), [GitHub](https://github.com/Dokploy/dokploy)
- Easypanel: [docs](https://easypanel.io/docs), [MCP server](https://easypanel.io/docs/mcp), [pricing](https://easypanel.io/pricing)
- CapRover: [docs](https://caprover.com/docs/get-started.html), [Pro features](https://caprover.com/docs/troubleshooting-pro.html), [license](https://github.com/caprover/caprover/blob/master/LICENSE), [GitHub](https://github.com/caprover/caprover)
- Dokku and Kamal: [dokku.com](https://dokku.com/), [Dokku Pro](https://pro.dokku.com/), [kamal-deploy.org](https://kamal-deploy.org/)
