# Moving a server to Tugport: instructions for AI agents

You are an AI agent, and the person you work for asked you to move the apps on their Linux server into [Tugport](https://tugport.com), a self-hosted deploy platform that runs as one binary next to Docker. Follow these steps in order. The move should cost the apps at most a few seconds of downtime each, and nothing old is deleted until the person says so.

The full API is described at https://github.com/backmeupplz/tugport/blob/main/docs/API.md. Read it before step 4.

## Ground rules

- Look before you change anything, and show the person your plan before step 2.
- Stop old things, never delete them. Old containers, volumes, configs and data directories stay until the person has checked the new setup and tells you to clean up.
- Never paste secret values (env values, passwords, keys) into the chat. Move them from the server to Tugport on the server itself.
- If a step fails and you cannot see why, stop and ask instead of improvising around it.

## 1. Look around (read only)

If the person has not told you how to reach the server, ask for its SSH address and user. Connect as root (or a user with sudo) and find out what runs there:

- the OS and CPU (`cat /etc/os-release`, `uname -m`); Tugport needs Linux with systemd on x86_64 or arm64;
- containers and stacks: `docker ps -a`, `docker compose ls`, the compose files and `.env` files they use, `docker volume ls`, and `docker inspect` for mounts, ports, env names and restart policies;
- apps outside Docker: `systemctl list-units --type=service --state=running`, pm2, supervisor, cron jobs;
- what listens where: `ss -tlnp`;
- the reverse proxy (nginx, Caddy, Traefik, Apache) and its config, which gives you the map of domains to apps and ports;
- databases running natively (Postgres, MySQL, Redis) and which apps use them.

If the server runs Easypanel or Coolify, stop here: Tugport moves those by itself with `tugport migrate`, as described at https://github.com/backmeupplz/tugport/blob/main/docs/MIGRATION.md. Install Tugport (step 2), run `tugport migrate easypanel --dry-run` or `tugport migrate coolify --dry-run`, show the person the plan, and run it without `--dry-run` once they agree.

Otherwise write the plan: for every app, what it becomes in Tugport (step 4), where its data lives, which domains point at it, and the order you will move things in. Ask the person to confirm it.

## 2. Install Tugport

Run as root:

```sh
curl -fsSL https://tugport.com/install | sh
```

Without a terminal the installer answers yes to its questions, so it installs Docker when it is missing. It then:

- prints a **recovery key**. Give it to the person word for word and ask them to store it in their password manager. It is shown only once and is the only way to read the secrets and backups if the server's disk or TPM is lost. Do not keep a copy anywhere else;
- prints a **setup link** like `http://<server address>:8484/setup#...`.

Tugport's proxy wants ports 80 and 443. While the old proxy holds them, Tugport waits and takes them over the moment they are free, and it requests no certificates until then, so the sites keep running on the old proxy for now.

## 3. Get a key

Give the setup link to the person and ask them to:

1. open it and create the owner account (if port 8484 is firewalled, they can run `ssh -L 8484:localhost:8484 root@<server>` and open the link with `localhost` instead of the IP);
2. turn on two-factor sign-in from the account menu;
3. open **Access**, choose **New agent**, give it the **admin** role on all projects and the **destroy** grant, and send you the key (`tpk_...`). The destroy grant lets you attach the server's existing volumes and folders, so data stays where it is; they can remove the grant or revoke the key after the move.

Use the key on the server against `http://127.0.0.1:8484`, so it never crosses the internet unencrypted:

```sh
export TUGPORT_URL=http://127.0.0.1:8484 TUGPORT_TOKEN=tpk_...
curl -s -H "Authorization: Bearer $TUGPORT_TOKEN" $TUGPORT_URL/api/me
```

The same binary is a CLI (`tugport status`, `tugport deploy`, `tugport logs`, `tugport env`), and it reads those two variables.

## 4. Recreate every app

Create a project for each group of apps that belong together (`POST /api/projects`), then a service for each app (`POST /api/projects/{project}/services`):

- **A container from an image:** an `app` with `source: {type: "image", image}`, its env (`env` in the create call, or `PUT /api/services/{id}/env`), its command if it overrides one, and its `mounts`. Attach the volume or folder it already uses (`{type: "volume", name}` or `{type: "bind", source}`), so no data is copied.
- **Code built on the server or from a repository:** an `app` with a `git` source, built with its Dockerfile or with Railpack or Nixpacks. A private repository needs a deploy key (`POST /api/services/{id}/deploy-key`, which the person adds to the repository) or a connected Git host.
- **A Compose stack:** either split it into apps and databases, which gives each its own logs, charts and rollbacks, or keep it as a `compose` service with the file in `source: {type: "compose", yaml}`. A stack that reuses outside volumes or host folders needs `privileged: true` in its spec.
- **A database:** either keep its data where it is, as an `app` running the same image and version on the old volume or data directory, or create a Tugport `database` service (`kind: "database"`, `database: {engine}`) and move the data with a dump and restore. Its container's name is in `GET /api/services/{id}` (`status.replicas[].name`), so you can run the restore with `docker exec` on the server. A new database adds a `<NAME>_URL` project variable that the project's apps receive.
- **A static site:** an `app` from a Dockerfile that copies the files into a small web server image, or the files mounted read-only into `nginx`.

Services reach each other by service name on their project's network. A container that talks to a database through `localhost` or the host's address needs its connection setting changed to the service name.

Move one app at a time. For an app that writes to a volume or a database, stop the old copy first (`docker stop`, `docker compose stop` or `systemctl stop`), so two copies never write the same data, then deploy the new one with `POST /api/services/{id}/deploy` and `{"wait": true}`. Stateless apps can start next to their old copies. Check each one with `GET /api/services/{id}` (its state should be `running`) and its logs (`GET /api/services/{id}/logs`) before you go on.

## 5. Switch the domains

Add every domain to its service with `POST /api/services/{id}/domains` and `{host, port}`, where `port` is the port the app listens on inside its container. Copy basic auth, redirects and body size limits from the old proxy config into the domain's `options`.

When every service runs, stop the old proxy and keep it from starting at boot (for nginx: `systemctl stop nginx && systemctl disable nginx`). Tugport's proxy takes ports 80 and 443 within half a second and requests a Let's Encrypt certificate for each domain within the next minute. Then check every domain from outside with `curl -sI https://<domain>`, and stop the old copies of the apps that still run.

To undo the switch, start the old proxy again after `systemctl stop tugport`.

## 6. Finish

Tell the person:

- what moved and what did not, and why;
- that the old containers, volumes and configs are stopped but still there, and what you would remove once they are happy;
- that they should keep the recovery key safe, and may now remove the agent's destroy grant or revoke its key;
- that Tugport can back up databases and volumes on a schedule (**Settings → Backups**), and offer to add a job for each one.

## Moving to a new server instead

The steps are the same, except that you install Tugport on the new server and copy the data across before each app starts there: dump and restore databases, and copy volumes and folders with `tar` over SSH. Ask the person to lower the domains' DNS TTL a day ahead, and point DNS at the new server as the last step instead of stopping the old proxy.
