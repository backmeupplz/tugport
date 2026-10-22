# Moving from Easypanel or Coolify

`tugport migrate easypanel` and `tugport migrate coolify` move a server's projects, services, domains, certificates, environment, volumes and deploy webhooks into Tugport. The installer runs `tugport migrate detect` and offers the move when it finds a panel.

| Command | What it does |
|---|---|
| `tugport migrate detect` | Prints the panels found here (`easypanel 2.38.0`). |
| `tugport migrate easypanel --dry-run` | Prints the plan: every service, what maps, what needs attention, what does not move. Changes nothing and never prints secret values. |
| `tugport migrate easypanel [--yes]` | Runs the move. Asks first unless `--yes`. |
| `tugport migrate rollback [--yes]` | Undoes it: the old panel, its proxy and services run again. |

Tugport must be installed and running: its proxy waits for ports 80 and 443 and takes them over at the switch.

## Reading the old panel

- **Easypanel** keeps its state in LMDB (`/etc/easypanel/data`). Tugport reads it with Easypanel's own lmdb binding inside the running `easypanel` container (a read-only snapshot), or with the same image on a copy of the file when the panel is stopped. Certificates come from Traefik's `acme.json`, the images the services run from the swarm.
- **Coolify** keeps its state in Postgres. Tugport queries it with `psql` inside `coolify-db` and decrypts Laravel's encrypted columns (AES-256-CBC with an HMAC, key from `APP_KEY` in `/data/coolify/source/.env`). `{{project.X}}`-style shared variables are resolved. The containers Coolify runs supply the image, mounts, command and old names.

## How things map

| Old | Tugport |
|---|---|
| Project (Coolify: project and environment; production keeps the project's name) | Project, with project-level env as shared variables |
| App from GitHub, Git, image or Dockerfile | App with the same source. The first deploy runs the image the old service runs, so nothing is rebuilt during the move. |
| Nixpacks / Railpack build settings | Same builder; install, build and start commands become `NIXPACKS_*` / `RAILPACK_*` variables |
| Postgres, MySQL, MariaDB, MongoDB, Redis | Database service on the same data directory or volume, same credentials and image |
| Compose stack | Compose service; named volumes point at the old stack's volumes |
| Env vars, file mounts | Encrypted env vars and files |
| Volume and bind mounts | The same Docker volumes and host paths, so no data is copied |
| Domains, basic auth, redirects, maintenance page, www redirect | Domains with the same options |
| Let's Encrypt certificates | Imported, so nothing is re-issued |
| Easypanel deploy webhook `http://IP:3000/api/deploy/<token>` | Keeps working: Tugport answers on `:3000` after the switch, and GitHub pushes to that URL deploy only the deployed branch |
| GitHub token (Easypanel) | A `github_token` Git provider |
| `zeroDowntime: false`, published ports | `stop_first` rollout |
| Service names with `_` | Renamed (`cas_ban_bot` becomes `cas-ban-bot`); every old hostname (`bots_cas_ban_bot`, `bots-cas-ban-bot`, `cas_ban_bot`, Coolify's uuids and container names) stays an alias |
| A service mentioning another project's service by its old name | The referenced service also joins the referencing project's network |

Not moved, and listed in the plan: Easypanel Box and WordPress services, wildcard domains, custom Traefik middlewares and hand-written Traefik routes, database admin tools, backup schedules, notification channels, users, Coolify's API-token deploy webhooks, KeyDB, Dragonfly and ClickHouse.

## The cutover

Every step is recorded in a journal (the `migration` state key). A rerun resumes where the last one stopped; `rollback` undoes the steps in reverse.

1. **Import.** Projects, services, env, files, domains, certificates and webhook tokens become Tugport rows. Nothing runs yet. Imported services keep `no_new_privileges: false` so they behave exactly as before.
2. **Databases, one at a time.** The old copy stops (Easypanel: scaled to zero; Coolify: stopped) and is gone before Tugport's starts on the same data, so no data is ever written by two engines. Tugport's copy then joins the old panel's networks under the old names (Easypanel's overlays are attachable), so the old apps keep reaching it. Each database is down for a few seconds.
3. **Apps start next to their old copies,** from the image the old copy runs. The old proxy still serves traffic.
4. **Services that must never run twice** (stop-first, published ports, compose stacks) move like databases, right before the switch.
5. **Switch.** The old proxy stops (`easypanel-traefik` scaled to zero, or `coolify-proxy` stopped) and Tugport's proxy binds ports 80 and 443 within half a second. If it does not answer within 30 seconds, the old proxy starts again and the move stops. Then the old apps and the old panel stop, Tugport serves the API on `0.0.0.0:3000` for Easypanel's webhook URLs, and Tugport's copies leave the old networks.

Old containers and swarm services are stopped, never removed, and Tugport's image cleanup leaves their images alone, so `rollback` can start them again: old apps start next to Tugport's copies, Tugport stops so the old proxy gets ports 80 and 443 back, databases move back one at a time (Tugport's copy first), the old panel starts, and the imported rows are deleted. Volumes are never deleted.
