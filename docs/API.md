# Tugport API

All endpoints live under the panel address (`http://<server>:8484` or the panel domain) and speak JSON. Errors look like `{"error": "message"}` with a matching HTTP status.

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
- **`destroy`** is required to delete things and to enable privileged mode, the Docker socket, host networking, bind mounts or extra capabilities.

The owner can do everything.

Services can be addressed by id or as `project.service` (for example `shop.api`). Projects can be addressed by id or by name.

Endpoints marked **(planned)** are not implemented yet; their shapes are the contract.

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

## Users, agents and keys

| Method | Path | Notes |
|---|---|---|
| GET | `/api/users` | `{users: [{id, kind, name, email, is_owner, all_projects_role, projects: {project_id: role}, grants, disabled, totp, created_at}]}`. Non-owners only see id, kind and name for other users. |
| POST | `/api/users` | Owner only. `{kind: "human"\|"agent", name, email?, password?, description?, all_projects_role?, projects?: {project: role}, grants?: []}` |
| GET / PATCH / DELETE | `/api/users/{id}` | PATCH accepts `{name?, email?, password?, description?, disabled?, all_projects_role?, projects?, grants?}` |
| POST | `/api/agents` | Owner only. Creates an agent and its key in one step. |
| GET | `/api/users/{id}/keys` | `{keys: [{id, name, prefix, expires_at, allowed_ips, last_used_at, last_used_ip, created_at, revoked_at}]}` |
| POST | `/api/users/{id}/keys` | `{name?, expires_days?, allowed_ips?}` returns `{id, token, snippets}` |
| DELETE | `/api/keys/{id}` | Revokes the key. |
| POST | `/api/keys/{id}/rotate` | Returns `{id, token, snippets}`; the old key is revoked. |
| GET | `/api/audit?project&service&actor&before&limit` | `{entries: [{id, at, actor, actor_kind, key_id, ip, action, project_id, service_id, target, detail}]}` |

`POST /api/agents` takes and returns:
- **Request body:** `{name, description?, all_projects_role?, projects?: {project: role}, grants?: ["secrets:read", "destroy"], key?: {name?, expires_days?, allowed_ips?: ["10.0.0.0/8"]}}`.
- **Response:** `{user, key: {id, token}, snippets: {env, cli, claude_code, codex, curl}}`. The token is shown only once.

## Projects

| Method | Path | Notes |
|---|---|---|
| GET | `/api/projects` | `{projects: [{id, name, description, services, role, created_at}]}` |
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

It returns `{id, name, kind, spec, deploy_hook, database?: {url, project_var}}`. A new database also adds a `<NAME>_URL` project variable.

### The service spec

Every field has a default. See `src/model.rs`.

| Field | Shape |
|---|---|
| `source` | `{type: "git", provider?, repo, branch, path, watch_paths, auto_deploy}`, `{type: "image", image}`, `{type: "dockerfile", content}` or `{type: "compose", yaml}` |
| `build` | `{type: "auto"}`, `{type: "dockerfile", path, target?}`, `{type: "railpack"}`, `{type: "nixpacks"}` or `{type: "buildpacks", builder}` |
| Runtime basics | `command`, `entrypoint`, `working_dir`, `user`, `replicas` |
| `ports` | `[{container, host, protocol: tcp\|udp}]` |
| `mounts` | `[{type: volume, name, target, readonly}, {type: bind, source, target, readonly}, {type: file, target}]` |
| `resources` | `{cpus, memory_mb, memory_reservation_mb}` |
| `health` | `{kind: auto\|http\|tcp\|command\|none, path, port, command, interval_s, timeout_s, retries, start_period_s}` |
| `rollout` | `{strategy: start_first\|stop_first, drain_s, health_timeout_s, keep_images}` |
| `deploy_policy` | `cancel` or `queue` |
| Container options | `gpu`, `cap_add`, `cap_drop`, `groups`, `sysctls`, `privileged`, `docker_socket`, `host_network`, `no_new_privileges` |
| `auto_update` | `{enabled, interval_minutes, policy: digest\|same_major}` |
| `placement` | `{node, region, labels}` |
| `autoscale` | `{min, max, cpu_percent, rps}` |
| `sleep` | `{idle_minutes}` |
| Other | `database`, `compose: {file}`, `build_args` |

### Service endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/api/services/{id}` | `{service, status, domains, env: [names], files: [targets], last_deploy, role}` |
| PATCH | `/api/services/{id}` | `{spec: <JSON merge patch>}` returns `{spec, redeploy_needed}` |
| DELETE | `/api/services/{id}?volumes=true` | Needs `destroy`. |
| GET | `/api/services/{id}/env` | `{env: [names] or {NAME: value}, project_vars: [names], values_visible}` |
| PUT | `/api/services/{id}/env` | `{NAME: "value" \| null}`. Writing never needs `secrets:read`. |
| GET / PUT | `/api/services/{id}/files` | PUT takes `{target: "/etc/app.conf", content \| null}` |
| GET / POST | `/api/services/{id}/domains` | POST takes `{host, path?, port, container?, https?, via_tunnel?, options?}` |
| PATCH / DELETE | `/api/domains/{id}` | |
| POST | `/api/services/{id}/hook` | Issues a new deploy webhook URL `{url}`; the old one stops working. |
| POST | `/api/services/{id}/deploy-key` | `{public_key}`: an SSH key to add to the repository. |

`status` looks like `{state, running, desired, live_deploy, replicas: [{id, name, deploy, replica, state, health, ip, image, created, node}]}`. `state` is one of:
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
- `basic_auth: [{user, hash}]`
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
- `trigger` is `manual`, `api`, `agent`, `push`, `webhook`, `rollback`, `autoupdate` or `migration`.

`GET /api/events` streams events (the event name is the `kind`; data is `{kind, project_id, service_id, data, at}`). The kinds are:
- `deploy`
- `deploy.log`
- `service.status`
- `service.updated`
- `audit`
- `certificate`
- `settings`
- `metrics`
- `warning`

## Hooks (no auth; the URL is the secret)

| Method | Path | Notes |
|---|---|---|
| POST / GET | `/api/hooks/deploy/{token}?ref=` | Deploys the service. |
| POST / GET | `/api/deploy/{token}` | The same, at Easypanel's path, so migrated webhooks keep working. |
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

To connect a GitHub App, `POST /api/git/github/manifest` takes `{organization?}` and returns `{action, manifest}`. The browser must then POST a form to `action` with a field `manifest` set to `JSON.stringify(manifest)`.

## Settings

| Method | Path | Notes |
|---|---|---|
| GET | `/api/settings` | Owner only. |
| PATCH | `/api/settings` | Owner only; takes a JSON merge patch. |

The settings fields are:
- `panel_domain`
- `panel_listen`
- `acme_email`
- `deploy_policy`
- `docker_cleanup: {enabled, interval_hours, build_cache_gb}`
- `log_cleanup: {enabled, max_gb, max_days}`
- `trusted_proxies`
- `builds: {max_parallel, cpus, memory_mb}`
- `alerts: {cpu_percent, memory_percent, disk_percent, gpu_temp_c, for_minutes}`
- `updates: {auto, channel}`

## Logs (planned)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/services/{id}/logs?since&until&q&limit&replica` | `{lines: [{ts, replica, stream, line}]}`. Times are epoch ms; `q` is a case-insensitive substring. |
| GET | `/api/services/{id}/logs/stream` | SSE `line` events with `{ts, replica, stream, line}` |
| GET | `/api/logs/tugport?limit&level` | `{lines: [{ts, level, target, message}]}` (Tugport's own log) |
| GET | `/api/logs/usage` | `{total_bytes, services: [{id, bytes}]}` |

## Metrics (planned)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/metrics/now` | Current snapshot, see below. |
| GET | `/api/metrics?scope=host\|service:<id>\|gpu:<n>&names=cpu,memory&range=1h\|6h\|24h\|7d\|30d` | `{step, series: {name: [[ts, value]]}}` |
| GET | `/api/docker/usage` | `{images, build_cache, volumes, containers}` (bytes) |

`GET /api/metrics/now` returns `{cpu, cores: [..], load: [1,5,15], memory: {used, total}, swap: {used, total}, disks: [{mount, used, total}], net: {rx, tx}, gpus: [{name, util, mem_used, mem_total, temp, power}], services: [{id, cpu, memory}]}`.

Series names:
- **`host`:** `cpu`, `memory`, `swap`, `load1`, `disk:<mount>`, `disk_read`, `disk_write`, `net_rx`, `net_tx`, `temp`
- **`service:<id>`:** `cpu`, `memory`, `net_rx`, `net_tx`, `restarts`
- **`gpu:<n>`:** `util`, `memory`, `temp`, `power`

## Console (planned)

| Method | Path | Notes |
|---|---|---|
| WS | `/api/services/{id}/console?container=<id>` | Text frames carry terminal I/O; send `{"resize": [cols, rows]}` as a text frame starting with `{`. Needs `operator`. |

## Notifications (planned)

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

## Registries (planned)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/registries` | `{registries: [{id, host, username}]}` |
| POST | `/api/registries` | `{host, username, password}` |
| DELETE | `/api/registries/{id}` | |

## Backups (planned)

| Method | Path | Notes |
|---|---|---|
| GET / POST | `/api/backups/targets` | POST takes `{name, kind: s3\|local, config}`. S3 config: `{endpoint, bucket, prefix?, access_key, secret_key, region?}`. Local config: `{path}`. |
| DELETE | `/api/backups/targets/{id}` | |
| GET / POST | `/api/backups/jobs` | POST takes `{service_id?, target_id, kind: database\|volume\|state, volume?, schedule (cron), keep_last}` |
| PATCH / DELETE | `/api/backups/jobs/{id}` | |
| POST | `/api/backups/jobs/{id}/run` | |
| GET | `/api/backups/jobs/{id}/runs` | `{runs: [{id, kind, status, snapshot, size_bytes, error, started_at, finished_at}]}` |
| POST | `/api/backups/runs/{id}/restore` | `{test?: true}` restores into a throwaway copy and reports. |

## Cloudflare Tunnel (planned)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/tunnel` | `{enabled, account, tunnel_id, status}` |
| PUT | `/api/tunnel` | `{api_token}`: creates the tunnel and runs cloudflared. |
| DELETE | `/api/tunnel` | |

Domains with `via_tunnel: true` are routed through the tunnel.

## Servers (planned, milestone 2)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/nodes` | `{nodes: [{id, name, region, labels, status, mesh_ip, info, last_seen_at}]}` |
| POST | `/api/nodes/join-token` | `{command}`: the one-line install command for a new server. |
| PATCH / DELETE | `/api/nodes/{id}` | |

## Cloud (planned, milestone 3)

| Method | Path | Notes |
|---|---|---|
| GET / POST | `/api/cloud/accounts` | POST takes `{provider: aws\|hetzner, name, credentials}` |
| POST | `/api/cloud/servers` | `{account_id, region, size, name?}` creates a server that joins automatically. |
| GET / PUT | `/api/cloud/autoscale` | `{account_id, region, size, min, max, scale_up_cpu, scale_down_cpu}` |

## MCP (planned)

`POST /mcp` speaks streamable HTTP JSON-RPC (`initialize`, `tools/list`, `tools/call`). It uses the same bearer key and the same permissions as the REST API.
