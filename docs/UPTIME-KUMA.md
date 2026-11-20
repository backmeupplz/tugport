# Deploy Uptime Kuma on a Hetzner VPS with Tugport

You run Uptime Kuma on Tugport straight from its Docker image: point a service at `louislam/uptime-kuma:2`, mount a volume at `/app/data`, attach a domain, and deploy. On a cold server, the deploy finished in 73 seconds and served HTTPS on the first hit with a fresh Let's Encrypt certificate.

Tugport deliberately skips one-click app templates, so you pass the image, volume, and port in three API calls.

**Tested on October 28, 2026** with Tugport 0.1.30, Docker 29.9.2, and Uptime Kuma 2.5.8 (Node.js 22.23.4) on a Hetzner dedicated server already running Tugport. Step 1 comes from the README and installer output rather than a fresh VPS, and I ran Steps 2 to 7 directly against the panel's API (the same endpoints behind the New service form) instead of clicking through the UI.

## What you need

- A Hetzner Cloud VPS (or any Linux box with systemd) running Tugport. A cheap small box works fine: with one monitor running, Uptime Kuma sat around 295 MB of RAM and under 1% of one CPU core right after a redeploy.
- A domain where you can add an A record.
- An API key with the `operator` role on your project (grab it under Access in the panel). In the commands below, `$PANEL` is your panel URL and `$TOKEN` is that key.

## Step 1: Install Tugport on the server

On the VPS, run the installer from the [README](https://github.com/backmeupplz/tugport):

```sh
curl -fsSL https://tugport.com/install | sh
```

It installs Docker if it's missing, starts Tugport as a system service and prints a one-time link for creating the owner account. Ports 80 and 443 serve your sites, and 8484 serves the panel until you give the panel its own domain.

## Step 2: Create the Uptime Kuma service

Create a service in a project (here `monitoring`) from the Docker image, with a named volume for Uptime Kuma's data folder:

```sh
curl -sS -X POST "$PANEL/api/projects/monitoring/services" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"name": "kuma", "kind": "app",
       "spec": {"source": {"type": "image", "image": "louislam/uptime-kuma:2"},
                "mounts": [{"type": "volume", "name": "kuma-data", "target": "/app/data"}]}}'
```

The answer returned in under a second with the service's `id` and its full spec, and nothing was running yet. The image tag `2` follows Uptime Kuma's 2.x line. Pin an exact tag such as `2.5.8` if you want updates only when you ask for them.

Uptime Kuma keeps its database, settings and monitors in `/app/data`. Without the volume, every redeploy would start from an empty setup screen.

## Step 3: Point a domain at the server

Add an A record for your hostname (here `kuma.example.com`) that points at the server's IP address. If the domain is on Cloudflare, leave the record on DNS only, so Tugport can get its certificate. Then add the domain to the service. Uptime Kuma listens on port 3001:

```sh
curl -sS -X POST "$PANEL/api/services/monitoring.kuma/domains" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"host": "kuma.example.com", "port": 3001}'
```

Services can be addressed as `project.service`, so you don't need the id. The answer has `"https": true`: Tugport requests the certificate itself.

## Step 4: Deploy

```sh
curl -sS -X POST "$PANEL/api/services/monitoring.kuma/deploy" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"wait": true}'
```

`wait: true` holds the call until the deploy finishes. On my server the first deploy took 73 seconds, mostly pulling the image's layers, and its status was `live`. The service's replica reported `healthy` from the automatic HTTP health check on port 3001, and the first request to `https://kuma.example.com/` came back as a redirect to `/setup-database` over a valid Let's Encrypt certificate.

## Step 5: Finish Uptime Kuma's setup

Open the domain in a browser. Uptime Kuma 2.x first asks which database to use:

1. Choose **SQLite** for a single instance; it writes straight to the Step 2 volume with zero extra services (embedded MariaDB and external MariaDB/MySQL are the other options).
2. Click **Next** and let it initialize the database.
3. Pick a username and strong password for the admin account.

You land on an empty dashboard that says "No Monitors, please add one".

## Step 6: Add a first monitor

Press **Add New Monitor**, leave the type on HTTP(s), give it a name, paste the URL you want to watch, and save. I added `https://tugport.com` with the default check every 60 seconds, and the monitor showed Up on the dashboard within a few seconds.

Alerts go through Uptime Kuma's own notification settings (email, Telegram, webhooks and many more), under Settings. I did not set up an alert for this page.

## Step 7: Check that your data survives a redeploy

Redeploy the same service again:

```sh
curl -sS -X POST "$PANEL/api/services/monitoring.kuma/deploy" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"wait": true}'
```

This one took 51 seconds. Afterwards the dashboard still showed the monitor and my browser was still signed in, because Uptime Kuma's data lives in the `kuma-data` volume and not in the container. The service's `rollout` setting defaults to `start_first`, so the new container has to pass its health check before it takes traffic.

## Before you rely on it

Don't monitor a server from itself. If that machine or its network drops, Uptime Kuma goes dark right alongside the apps it's watching and can't alert you, so put it on a separate server or provider for anything you care about.

Set up scheduled backups for `kuma-data` to disk or S3-compatible storage using Tugport's built-in backup support ([API reference](/docs/api/)); I haven't tested that restore flow myself here.

## Frequently asked questions

### Does Uptime Kuma need a database server?

No. Pick SQLite in Step 5 and Uptime Kuma stores its data in a file in the data volume, with nothing else to run.

### How much memory does Uptime Kuma use?

On my server, right after the second deploy and with one monitor, Tugport's metrics showed about 295 MB of memory and 0.7% of one core. Expect more with many monitors.

### How do I update Uptime Kuma?

Deploy the service again to pull and run the image. A tag such as `2` moves with Uptime Kuma's releases. The service's `auto_update` setting can check for a new image on a schedule ([API reference](/docs/api/)); I did not test it for this page.
