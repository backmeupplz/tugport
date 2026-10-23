# Tugport API

All endpoints live under the panel address (`http://<server>:8484` or the panel domain) and speak JSON. Every error, including a malformed body, looks like `{"error": "message"}` with a matching HTTP status.

Work that takes a while (backups, restores, removing a server, adding a database replica) finishes even if the caller disconnects.

## Authentication

- **People** sign in with `POST /api/auth/login` and get an HttpOnly `tp_session` cookie.
- **Agents and scripts** send `Authorization: Bearer tpk_...`.

Every request is checked against the caller's role on the project it touches:

| Role | What it adds |
|---|---|
| `viewer` | Status, charts, logs and the names of env vars |
| `deployer` | Deploy, restart, stop, start, roll back |
| `operator` | Change env, domains, mounts and settings; create services |
| `admin` | Delete things and manage project members |

Two grants sit on top of the roles:
- **`secrets:read`** lets the caller see env values.
- **`destroy`** is required to delete things and to enable privileged mode, the Docker socket, host networking, bind mounts, extra capabilities or extra networks, and to attach a volume that already exists on a server or belongs to another project.

The owner can do everything.

Services can be addressed by id or as `project.service` (for example `shop.api`). Projects can be addressed by id or by name.

## Setup and session

| Method | Path | Notes |
|---|---|---|
| GET | `/api/setup` | `{needs_setup}` |
| POST | `/api/setup` | `{token, name, email?, password}`: creates the owner (token from the setup link `#fragment`) and sets the session cookie |
| POST | `/api/auth/login` | `{name, password, totp?}`. A 401 with error `totp_required` means: ask for the code and retry. |
| POST | `/api/auth/logout` | |
| GET | `/api/me` | `{principal: {user_id, name, kind, is_owner, all_projects_role, project_roles, grants}, email, totp}` |
| POST | `/api/me/totp` | Starts 2FA setup: `{secret, url}` (otpauth URL for a QR code) |
| PUT | `/api/me/totp` | `{code}` confirms 2FA |
| DELETE | `/api/me/totp` | `{code}` turns 2FA off |
| GET | `/api/about` | `{version, commit, source, docker}` |
| GET | `/api/updates` | Owner only. `{current, latest, available}` |
| POST | `/api/updates/apply` | Owner only. Installs the newest release without dropping traffic: `{updating_to}` |

## Users, agents and keys

| Method | Path | Notes |
|---|---|---|
| GET | `/api/users` | `{users: [{id, kind, name, email, is_owner, all_projects_role, projects: {project_id: role}, grants, disabled, totp, created_at}]}`. Non-owners only see id, kind and name for other users. |
| POST | `/api/users` | Owner only. `{kind: "human"\|"agent", name, email?, password?, description?, all_projects_role?, projects?: {project: role}, grants?: []}` |
| GET / PATCH / DELETE | `/api/users/{id}` | PATCH accepts `{name?, email?, password?, description?, disabled?, all_projects_role?, projects?, grants?}`. Only the fields sent change; `all_projects_role: null` removes that role. People may change their own name, email, password and description; changing one's own password also needs `current_password`. |
| POST | `/api/agents` | Owner only. Creates an agent and its key in one step. |
| GET | `/api/users/{id}/keys` | `{keys: [{id, name, prefix, expires_at, allowed_ips, last_used_at, last_used_ip, created_at, revoked_at}]}` |
| POST | `/api/users/{id}/keys` | `{name?, expires_days?, allowed_ips?}` returns `{id, token, snippets}`. A key making a key for its own user passes on its expiry and address limits. |
| DELETE | `/api/keys/{id}` | Revokes the key. |
| POST | `/api/keys/{id}/rotate` | Returns `{id, token, snippets}`; the old key is revoked. Revoked keys cannot be rotated. |
| GET | `/api/audit?project&service&actor&before&limit` | `{entries: [{id, at, actor, actor_kind, key_id, ip, action, project_id, service_id, target, detail}]}` |

`POST /api/agents` takes and returns:
- **Request body:** `{name, description?, all_projects_role?, projects?: {project: role}, grants?: ["secrets:read", "destroy"], key?: {name?, expires_days?, allowed_ips?: ["10.0.0.0/8"]}}`.
- **Response:** `{user, key: {id, token}, snippets: {env, cli, claude_code, codex, curl}}`. The token is shown only once.

## Projects

| Method | Path | Notes |
|---|---|---|
| GET | `/api/projects` | `{projects: [{id, name, description, services, states: {running: 2, ...}, role, created_at}]}` |
| POST | `/api/projects` | `{name, description?}`. Names are lowercase DNS-safe. |
| GET | `/api/projects/{id}` | `{project, services: [{id, name, kind, source, status}], vars: [names], role}` |
| PATCH | `/api/projects/{id}` | `{description}` |
| DELETE | `/api/projects/{id}` | Needs `destroy`; the project must be empty. |
| GET | `/api/projects/{id}/vars` | `{vars: [names] or {NAME: value}, values_visible}`. Values are shown only with `secrets:read`. |
| PUT | `/api/projects/{id}/vars` | `{NAME: "value" \| null}`. `null` deletes the variable. |
| GET | `/api/projects/{id}/members` | `{members: [{id, name, kind, role, via_all_projects}]}` |
| PUT | `/api/projects/{id}/members/{user}` | `{role: "viewer" \| ... \| null}` |

## Services

### Creating a service

`POST /api/projects/{id}/services` takes:
- `name` and `kind` (`app`, `database` or `compose`);
- an optional `spec`;
- an optional `database: {engine: "postgres"|"mysql"|"mariadb"|"mongo"|"redis", user?, database?}`;
- optional `env: {NAME: value}`.

It returns `{id, name, kind, spec, deploy_hook, database?: {url, project_var}}`. A new database also adds a `<NAME>_URL` project variable. A service name whose container prefix (`tp-<project>-<name>`) another project's service already has is refused.

A Compose stack's file is checked at every deploy. Without the stack's `privileged` switch (which needs `destroy`, like for apps), it may only use Compose keys that stay inside the stack: no privileged mode, capabilities, devices, host namespaces (`network_mode`, `pid` and `ipc` may only share another service of the stack), providers or API sockets; bind mounts, `env_file`, secrets, configs and build contexts only inside the stack's folder and without `${VAR}`; no external or named networks and volumes, network drivers other than bridge, IPAM settings, `include` or `extends` from other files. A repository whose symlinks point outside it is refused for any build.

`GET /api/services` lists every service the caller can see: `{services: [{id, name, project, project_id, kind, state, running, desired}]}`.

### The service spec

Every field has a default. See `src/model.rs`.

| Field | Shape |
|---|---|
| `source` | `{type: "git", provider?, repo, branch, path, watch_paths, auto_deploy}`, `{type: "image", image}`, `{type: "dockerfile", content}` or `{type: "compose", yaml}` |
| `build` | `{type: "auto"}`, `{type: "dockerfile", path, target?}`, `{type: "railpack"}`, `{type: "nixpacks"}` or `{type: "buildpacks", builder}` |
| Runtime basics | `command`, `entrypoint`, `working_dir`, `user`, `replicas` (ignored while `autoscale` is set) |
| `ports` | `[{container, host, protocol: tcp\|udp}]` |
| `mounts` | `[{type: volume, name, target, readonly}, {type: bind, source, target, readonly}, {type: file, target}]`. Docker volumes are shared by everything on a server, so a volume a service adds must be new on every server or already used by this project, unless an admin with `destroy` attaches it. |
| `resources` | `{cpus, memory_mb, memory_reservation_mb}` |
| `health` | `{kind: auto\|http\|tcp\|command\|none, path, port, command, interval_s, timeout_s, retries, start_period_s}` |
| `rollout` | `{strategy: start_first\|stop_first, drain_s, health_timeout_s, keep_images}` |
| `deploy_policy` | `cancel` or `queue` |
| Container options | `gpu`, `cap_add`, `cap_drop`, `groups`, `sysctls`, `privileged`, `docker_socket`, `host_network`, `no_new_privileges` |
| `auto_update` | `{enabled, interval_minutes, policy: digest\|same_major}` |
| `placement` | `{node, region, labels}`: `node` is a server id or name; labels must all match. Services with a database, volumes or bind mounts, host ports, host networking or the Docker socket stay on the server they first ran on. Other services spread their replicas over the eligible servers. |
| `autoscale` | `{min, max, cpu_percent, rps}`: replicas follow total CPU (percent of one core per replica) or requests per second per replica. More start at once; extras leave after five quiet minutes. |
| `sleep` | `{idle_minutes}`: the service stops after that long without requests and starts again on the next one (web apps only). |
| Other | `database`, `compose: {file}`, `build_args`, `networks` (extra Docker networks to join; adding one needs `destroy`) |

### Service endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/api/services/{id}` | `{service, status, domains, env: [names], files: [targets], last_deploy, exposed_ports, role}`. `exposed_ports` are the ports the running image declares. |
| PATCH | `/api/services/{id}` | `{spec: <JSON merge patch>}` returns `{spec, redeploy_needed}` |
| DELETE | `/api/services/{id}?volumes=true` | Needs `destroy`. |
| GET | `/api/services/{id}/env` | `{env: [names] or {NAME: value}, project_vars: [names], values_visible}` |
| PUT | `/api/services/{id}/env` | `{NAME: "value" \| null}`. Writing never needs `secrets:read`. |
| GET / PUT | `/api/services/{id}/files` | PUT takes `{target: "/etc/app.conf", content \| null}` |
| GET / POST | `/api/services/{id}/domains` | POST takes `{host, path?, port, container?, https?, via_tunnel?, options?}`. A host belongs to one project, the panel's own domain to the panel, and turning on `via_tunnel` (which writes DNS records in the owner's Cloudflare account) needs the owner. |
| PATCH / DELETE | `/api/domains/{id}` | PATCH takes the same fields; `container: null` clears a Compose container target. |
| GET | `/api/services/{id}/hook` | The deploy webhook URL `{url}`. Needs `operator` and `secrets:read`. |
| POST | `/api/services/{id}/hook` | Issues a new deploy webhook URL `{url}`; the old one stops working. |
| GET | `/api/services/{id}/replicas` | Databases: `{replicas: [{id, name, state, nodes, replication}]}`. `replication` is `copying`, `following` or `failed: <reason>`. |
| POST | `/api/services/{id}/replicas` | Databases, `operator`: `{placement?}` adds a read replica, see [Database replicas](#database-replicas). Returns `{replica: {id, name}, deploy_id}`. |
| POST | `/api/services/{id}/promote` | On a replica, `admin`: makes it a primary. Returns `{variable, message}`. |
| POST | `/api/services/{id}/deploy-key` | `{public_key}`: an SSH key to add to the repository. |

`status` looks like `{state, running, desired, live_deploy, replicas: [{id, name, deploy, replica, state, health, ip, image, created, node, node_name}]}`. `node` is the id of the server the container runs on (`local` is the panel's own) and `node_name` its name. `state` is one of:
- `running`
- `degraded`
- `unhealthy`
- `crashing`
- `stopped`
- `asleep`
- `not deployed`

Domain `options` contains:
- `www_redirect` (`to_www` or `to_apex`)
- `redirects: [{from (regex), to, permanent}]`
- `basic_auth: [{user, password?}]`: passwords are stored as Argon2 hashes and never returned. An entry sent without a password keeps that user's current one.
- `forward_auth: {url, response_headers}`
- `strip_prefix`
- `rewrite: {from, to}`
- `maintenance` (HTML)
- `error_pages: {502: html}`
- `timeout_s`
- `max_body_mb`

## Deploys

| Method | Path | Notes |
|---|---|---|
| POST | `/api/services/{id}/deploy` | See below. |
| GET | `/api/services/{id}/deploys` | `{deploys: [...], live}` |
| GET | `/api/deploys/{id}` | `{deploy, log: [{ts, line}]}` |
| GET | `/api/deploys/{id}/log` | SSE stream: `line` events (`{ts, line}`) and `status` events. Ends after the final status. |
| POST | `/api/deploys/{id}/cancel` | |
| POST | `/api/services/{id}/rollback` | `{deploy_id?}` (defaults to the previous live image), returns `{deploy_id, image}` |
| POST | `/api/services/{id}/restart` / `stop` / `start` | |
| GET | `/api/events?project&service` | SSE stream, see below. |

`POST /api/services/{id}/deploy` takes `{ref?, wait?, timeout_s?}`:
- **Without `wait`:** returns `{deploy_id, status: "queued"}` straight away.
- **With `wait: true`:** blocks until the deploy finishes and returns `{deploy, log_tail}`.

A deploy record looks like `{id, service_id, status, trigger, actor, ref, commit_sha, commit_message, image, error, created_at, started_at, finished_at}`:
- `status` is `queued`, `building`, `deploying`, `live`, `failed`, `cancelled` or `superseded`.
- `trigger` is `manual`, `api`, `agent`, `push`, `webhook`, `rollback`, `autoupdate`, `migration`, `reschedule` (its server went away), `drain` (its server is being removed), `replica` or `replication`.

`GET /api/events` streams events (the event name is the `kind`; data is `{kind, project_id, service_id, data, at}`). The kinds are:
- `deploy`
- `deploy.log`
- `service.status`: `{event, container, state, running, desired}` after every container start, exit, out-of-memory kill and health change; also `wake`, `sleep`, `scaled` and `replica` events
- `service.updated`
- `audit`
- `certificate`
- `settings`
- `metrics`
- `alert`
- `backup`
- `nodes`: a server joined, came online or went offline
- `warning`

## Hooks (no auth; the URL is the secret)

| Method | Path | Notes |
|---|---|---|
| POST / GET | `/api/hooks/deploy/{token}?ref=` | Deploys the service, at `ref` (a branch, tag or commit) when given. |
| POST / GET | `/api/deploy/{token}` and `/api/compose/deploy/{token}` | The same, at Easypanel's paths, so migrated webhooks keep working. GitHub push events sent there only deploy pushes to the service's branch, and deploy the branch itself, since such bodies are not signed. |
| POST | `/api/nodes/join` | Called by `tugport install --join` on a new server; the one-time token is the credential. |
| POST | `/api/hooks/git/{provider}` | Push events. The signature is checked; watch paths and `[skip deploy]` are honored. |

## Git providers

| Method | Path | Notes |
|---|---|---|
| GET | `/api/git/providers` | `{providers: [{id, kind, name, base_url, created_at}]}` |
| POST | `/api/git/providers` | `{kind: "forgejo"\|"gitea"\|"gitlab"\|"github_token", name?, base_url?, token}` |
| DELETE | `/api/git/providers/{id}` | |
| GET | `/api/git/providers/{id}/repos` | `{repos: ["owner/name"]}` |
| POST | `/api/git/github/manifest` | See below. |
| GET | `/api/git/github/callback` | GitHub redirects here; it stores the app and redirects to the install page. |

To connect a GitHub App, `POST /api/git/github/manifest` takes `{organization?}` and returns `{action, manifest}`. The browser must then POST a form to `action` with a field `manifest` set to `JSON.stringify(manifest)`. After installing the app, GitHub sends people back to `/settings/git`.

## Settings

| Method | Path | Notes |
|---|---|---|
| GET | `/api/settings` | Owner only. |
| PATCH | `/api/settings` | Owner only; takes a JSON merge patch. |

The settings fields are:
- `panel_domain`
- `panel_listen`
- `panel_public`: when false, the panel domain only serves webhook paths and server joins; reach the panel itself on `panel_listen` (a tailnet address, say)
- `legacy_listen`: an extra address that only serves deploy webhooks (`0.0.0.0:3000` keeps Easypanel's webhook URLs working)
- `acme_email`
- `deploy_policy`
- `docker_cleanup: {enabled, interval_hours, build_cache_gb}`
- `log_cleanup: {enabled, max_gb, max_days}`
- `trusted_proxies`
- `builds: {max_parallel, cpus, memory_mb}`
- `alerts: {cpu_percent, memory_percent, disk_percent, gpu_temp_c, for_minutes}`
- `updates: {auto, channel}`

## Logs

| Method | Path | Notes |
|---|---|---|
| GET | `/api/services/{id}/logs?since&until&q&limit&replica` | `{lines: [{ts, replica, stream, line}]}`. Times are epoch ms; `q` is a case-insensitive substring. Logs from every server are collected in one place, with secret values masked. |
| GET | `/api/services/{id}/logs/stream` | SSE `line` events with `{ts, replica, stream, line}` |
| GET | `/api/logs/tugport?limit&level` | `{lines: [{ts, level, target, message}]}` (Tugport's own log) |
| GET | `/api/logs/usage` | `{total_bytes, services: [{id, bytes}]}` |

## Metrics

| Method | Path | Notes |
|---|---|---|
| GET | `/api/metrics/now?node=<id>` | A server's current snapshot (the panel's own by default), see below. Callers without a role on all projects only get `{at, services}` for their own services. |
| GET | `/api/metrics?scope=host\|service:<id>\|gpu:<n>&names=cpu,memory&range=1h\|6h\|24h\|7d\|30d&node=<id>` | `{step, series: {name: [[ts, value]]}}`. `node` picks the server for host and GPU scopes; service scopes add up every server. |
| GET | `/api/docker/usage?node=<id>` | `{images, build_cache, volumes, containers}` (bytes) on a server |

`GET /api/metrics/now` returns `{cpu, cores: [..], load: [1,5,15], memory: {used, total}, swap: {used, total}, disks: [{mount, used, total}], net: {rx, tx}, gpus: [{name, util, mem_used, mem_total, temp, power}], services: [{id, cpu, memory, net_rx, net_tx, restarts}]}`.

Units: `cpu` is percent (for services, percent of one core summed over replicas), memory is bytes, network and disk I/O are bytes per second, `disk:<mount>` is percent used, and `restarts` counts crashes per 10-second sample (stops Tugport makes are not crashes).

Series names:
- **`host`:** `cpu`, `memory`, `swap`, `load1`, `disk:<mount>`, `disk_read`, `disk_write`, `net_rx`, `net_tx`, `temp`
- **`service:<id>`:** `cpu`, `memory`, `net_rx`, `net_tx`, `restarts`
- **`gpu:<n>`:** `util`, `memory`, `temp`, `power`

## Console and exec

| Method | Path | Notes |
|---|---|---|
| WS | `/api/services/{id}/console?container=<id>` | Text frames carry terminal I/O; send `{"resize": [cols, rows]}` as a text frame starting with `{`. Needs `operator` and `secrets:read` (a shell can read the environment). A refusal arrives on the socket: one `{"error": "..."}` message, then close code 4403. |
| POST | `/api/services/{id}/exec` | `{command, timeout_s?, container?}` runs one command with the service's environment: `{exit_code, timed_out, output}`. Same permissions as the console. |

## Notifications

| Method | Path | Notes |
|---|---|---|
| GET | `/api/notifications` | `{channels: [{id, kind, name, events, enabled}]}` |
| POST | `/api/notifications` | `{kind: telegram\|email\|webhook, name, config, events: ["*"]}` |
| DELETE | `/api/notifications/{id}` | |
| POST | `/api/notifications/{id}/test` | |

The `config` for each kind:
- **telegram:** `{bot_token, chat_id, thread_id?}`
- **email:** `{host, port?, tls?, username?, password?, from, to}`
- **webhook:** `{url}`

## Registries

| Method | Path | Notes |
|---|---|---|
| GET | `/api/registries` | `{registries: [{id, host, username}]}` |
| POST | `/api/registries` | `{host, username, password}` |
| DELETE | `/api/registries/{id}` | |

## Backups

| Method | Path | Notes |
|---|---|---|
| GET / POST | `/api/backups/targets` | POST takes `{name, kind: s3\|local, config}`. S3 config: `{endpoint, bucket, prefix?, access_key, secret_key, region?}`. Local config: `{path}`. |
| DELETE | `/api/backups/targets/{id}` | |
| GET / POST | `/api/backups/jobs` | GET returns jobs with `enabled` and `last_run`. POST takes `{service_id?, target_id, kind: database\|volume\|state, volume?, schedule (cron), keep_last}`. |
| PATCH / DELETE | `/api/backups/jobs/{id}` | |
| POST | `/api/backups/jobs/{id}/run` | |
| GET | `/api/backups/jobs/{id}/runs` | `{runs: [{id, kind, status, snapshot, size_bytes, error, started_at, finished_at}]}` |
| POST | `/api/backups/runs/{id}/restore` | `{test?: true}` restores into a throwaway copy and reports. Returns `{ok, message}`. |

## Cloudflare Tunnel

| Method | Path | Notes |
|---|---|---|
| GET | `/api/tunnel` | `{enabled, account, tunnel_id, running, domains}` |
| PUT | `/api/tunnel` | `{api_token}`: creates the tunnel and runs cloudflared. The token needs Account > Cloudflare Tunnel > Edit and Zone > DNS > Edit. A connect that fails removes what it created. |
| DELETE | `/api/tunnel` | Stops cloudflared and deletes the tunnel and the DNS records Tugport made for it. |

Domains with `via_tunnel: true` are routed through the tunnel and get a proxied CNAME to it. A name that already has a CNAME Tugport did not make (or A/AAAA records) is left alone and reported. When a domain leaves the tunnel, Tugport deletes the record it made, unless someone has pointed that record elsewhere.

## Servers

Servers join over an encrypted WireGuard mesh (10.99.0.0/16, UDP 51820). Containers can never open connections into the mesh. Each server runs `tugport node`; the panel drives its Docker, sends it secrets (kept in its memory-only `/run`), routes, and collects its logs and metrics. Built images travel panel → server over that same channel, so there is no shared registry.

A server is either **ingress** or not. The panel is always ingress. Only ingress servers may terminate public traffic, so only they receive the TLS certificates, the sign-in (basic-auth) and forward-auth settings, and the full route table; a plain server gets only the routes for the services it hosts and serves nothing but requests forwarded from an ingress server. Turn a server into an ingress (under Servers, or `PATCH /api/nodes/{id}` with `ingress: true`) only when a domain's DNS points directly at it.

| Method | Path | Notes |
|---|---|---|
| GET | `/api/nodes` | `{mesh, nodes: [{id, name, local, ingress, address, mesh_ip, region, labels, status, info, last_seen_at, created_at}]}`. Needs the owner or a role on all projects. |
| POST | `/api/nodes/join-token` | Owner. `{panel_url?}` returns `{token, command, expires_at}`: a one-hour command to run as root on the new server. The first call turns on the mesh. |
| PATCH | `/api/nodes/{id}` | Owner. `{name?, region?, labels?, ingress?}`; `region: null` or `""` clears it. `ingress` has no effect on the panel (always ingress). |
| DELETE | `/api/nodes/{id}?keep_machine=true&force=true` | Owner. Moves the server's services elsewhere, removes it from the mesh and, if Tugport created it in a cloud account, deletes the machine unless `keep_machine`. Refused (409) while services keep their data there, unless `force`. Returns `{ok, machine_deleted}`. |

Joining never sends the one-time token: the new server sends an id derived from it and an HMAC that binds the token to its WireGuard key, and the panel's answer (the node key and swarm token) is sealed with a key derived from the token. A join over plain HTTP leaks nothing usable.

`status` is `online`, `unreachable` (a health check failed; already out of routing and placement), `offline` (30 seconds without answers), or `joining`. `info` has `{version, docker, cpus, memory_total, arch, nvidia, dri, swarm_node, boot_id, uptime_s}`.

A server that stays away for 2 minutes has its movable services redeployed on the others. Services that keep data there wait for it. When it comes back, replaced containers are removed and, after a reboot, secrets are sent again.

## Cloud

Owner only: these spend money.

| Method | Path | Notes |
|---|---|---|
| GET / POST | `/api/cloud/accounts` | POST takes `{kind: "hetzner", name, token}` or `{kind: "aws", name, access_key, secret_key}`. Credentials are checked with the provider first and kept sealed. |
| DELETE | `/api/cloud/accounts/{id}` | Refused while the account has servers or pools. |
| GET | `/api/cloud/accounts/{id}/options` | `{regions: [{id, name}], sizes: [{id, cpus, memory_gb, disk_gb?, arch, prices?}]}` |
| GET / POST | `/api/cloud/servers` | POST takes `{account_id, region, size, panel_url?}` and creates an Ubuntu 24.04 machine that installs Tugport and joins on first boot. A machine that has not joined after 20 minutes is deleted. |
| DELETE | `/api/cloud/servers/{id}` | Deletes a machine that has not joined (joined ones are removed under Servers). |
| GET / POST | `/api/cloud/pools` | POST takes `{name, account_id, region, size, labels?, min_servers?, max_servers?, scale_up_cpu?, scale_down_cpu?}` |
| PATCH / DELETE | `/api/cloud/pools/{id}` | Deleting a pool keeps its servers. |

A pool keeps at least `min_servers`, adds one server at a time while the average CPU of all servers stays above `scale_up_cpu` (10 minutes apart), and removes its newest server after 30 minutes below `scale_down_cpu`. It never removes a server that keeps data.

AWS machines use the default VPC and a `tugport` security group that opens UDP 51820 and TCP 80 and 443.

## Database replicas

A replica is a database service of its own, `<db>-replica-N`, placed like any service (another server or region; without a placement it goes to a different server than its primary). It follows its primary over the project network and refuses writes.

| Engine | How |
|---|---|
| Postgres | Streaming replication, seeded with `pg_basebackup` on first start. |
| MySQL | Binlog replication, seeded from a dump of the primary. |
| MariaDB | The same; the primary restarts once to turn on binary logging. |
| MongoDB | The primary restarts once as a one-member replica set; replicas join without votes, so the primary keeps working when they are away. |
| Redis | `replicaof` |

The project gets `<DB>_READ_URL` (for MongoDB, a replica-set URL that reads from secondaries) and `<DB>_REPLICA_N_URL` variables. Promoting a replica makes it a primary that stays one after restarts and points `<DB>_URL` at it; services read the new value on their next deploy or restart. Promote only when the primary is lost: if the old one still runs, both take writes. A primary with replicas cannot be deleted.

## MCP

`POST /mcp` speaks streamable HTTP JSON-RPC (`initialize`, `tools/list`, `tools/call`). It uses the same bearer key and the same permissions as the REST API (never a browser's session cookie). Its tools cover projects, services, deploys, logs, exec, env, domains, metrics, servers, backups, database replicas and the audit log.
