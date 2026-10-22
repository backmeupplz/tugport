<p align="center">
  <img src="website/logo.svg" width="96" alt="Tugport logo" />
</p>

<h3 align="center">Tugport</h3>
<p align="center">Deploy your apps and watch your server from one small Rust binary.</p>

---

Tugport is a self-hosted deploy platform in the spirit of Coolify and Easypanel, built as a single Rust binary that runs next to Docker. It is in early development, and nothing is ready to install yet.

## What it will do

- **Projects and services:** group apps and databases into projects, and give each service its own domains, automatic HTTPS, environment, volumes and limits.
- **Deploys on push:** build every push from GitHub or Forgejo with your Dockerfile or Nixpacks, switch traffic only after the health check passes, and roll back in one click.
- **Logs:** follow any service live and search history that survives restarts and redeploys.
- **Server charts:** CPU, memory, disk, network and GPU for the whole machine and for each service.
- **Agents as users:** create an agent user with a scoped API key in one step, limit what it may see and do per project, and audit everything it did.
- **Database backups:** scheduled backups to S3-compatible storage with in-place restore.

There is deliberately no app store.

## Repository

| Path | Contents |
|---|---|
| `website/` | tugport.com, deployed to GitHub Pages on every push to `main` |

The share image `website/og.png` is rendered from `website/og.html` with headless Chrome:

```sh
cd website && "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --hide-scrollbars \
  --window-size=1200,630 --virtual-time-budget=10000 --screenshot="$PWD/og.png" "file://$PWD/og.html"
```

## License

MIT
