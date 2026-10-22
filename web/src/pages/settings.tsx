import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";
import { del, get, isPlanned, patch, post, put, useApi, useMe, type Project } from "../api";
import { Action, Card, Check, Empty, ErrorBox, Field, Loaded, Modal, Note, Page, Pill, Planned, Select, Tabs, toast, useSubmit } from "../ui";
import { ago, bytes, cidr, mergePatch, num, when, words } from "../util";
import { PROVIDER_KINDS } from "./new-service";
import type { ProjectDetail } from "./projects";

const TABS: [string, string][] = [
  ["general", "General"],
  ["git", "Git"],
  ["notifications", "Notifications"],
  ["registries", "Registries"],
  ["backups", "Backups"],
  ["tunnel", "Cloudflare Tunnel"],
  ["about", "About"],
];

export function Settings(props: { tab?: string; path?: string }) {
  const me = useMe();
  const tab = TABS.some(([id]) => id === props.tab) ? (props.tab as string) : "general";
  const owner = me.principal.is_owner;
  return (
    <Page title="Settings">
      <Tabs label="Settings sections" current={tab} items={TABS.map(([id, label]) => [id, label, id === "general" ? "/settings" : `/settings/${id}`])} />
      <div class="max-w-3xl space-y-6">
        {!owner && tab !== "about" && tab !== "git" ? (
          <Note>Only the owner can change instance settings.</Note>
        ) : (
          <>
            {tab === "general" && <General />}
            {tab === "git" && <Git owner={owner} />}
            {tab === "notifications" && <Notifications />}
            {tab === "registries" && <Registries />}
            {tab === "backups" && <Backups />}
            {tab === "tunnel" && <Tunnel />}
            {tab === "about" && <About />}
          </>
        )}
      </div>
    </Page>
  );
}

// ---- General ----

type InstanceSettings = Record<string, any>;
const str = (v: unknown) => (v == null ? "" : String(v));

function toForm(s: InstanceSettings) {
  return {
    panelDomain: s.panel_domain ?? "",
    acmeEmail: s.acme_email ?? "",
    trusted: (s.trusted_proxies ?? []).join("\n"),
    policy: s.deploy_policy ?? "cancel",
    dcEnabled: !!s.docker_cleanup?.enabled,
    dcHours: str(s.docker_cleanup?.interval_hours),
    dcCache: str(s.docker_cleanup?.build_cache_gb),
    lcEnabled: !!s.log_cleanup?.enabled,
    lcGb: str(s.log_cleanup?.max_gb),
    lcDays: str(s.log_cleanup?.max_days),
    maxParallel: str(s.builds?.max_parallel),
    bCpus: str(s.builds?.cpus),
    bMem: str(s.builds?.memory_mb),
    aCpu: str(s.alerts?.cpu_percent),
    aMem: str(s.alerts?.memory_percent),
    aDisk: str(s.alerts?.disk_percent),
    aGpu: str(s.alerts?.gpu_temp_c),
    aFor: str(s.alerts?.for_minutes),
    upAuto: !!s.updates?.auto,
    upChannel: s.updates?.channel ?? "stable",
  };
}
type GeneralForm = ReturnType<typeof toForm>;

function toSettings(s: InstanceSettings, f: GeneralForm): InstanceSettings {
  return {
    ...s,
    panel_domain: f.panelDomain.trim().toLowerCase() || null,
    acme_email: f.acmeEmail.trim() || null,
    trusted_proxies: words(f.trusted).map(cidr),
    deploy_policy: f.policy,
    docker_cleanup: { ...s.docker_cleanup, enabled: f.dcEnabled, interval_hours: num(f.dcHours) ?? 6, build_cache_gb: num(f.dcCache) ?? 20 },
    log_cleanup: { ...s.log_cleanup, enabled: f.lcEnabled, max_gb: num(f.lcGb) ?? 10, max_days: num(f.lcDays) },
    builds: { ...s.builds, max_parallel: num(f.maxParallel) ?? 2, cpus: num(f.bCpus), memory_mb: num(f.bMem) },
    alerts: {
      ...s.alerts,
      cpu_percent: num(f.aCpu) ?? 90,
      memory_percent: num(f.aMem) ?? 90,
      disk_percent: num(f.aDisk) ?? 90,
      gpu_temp_c: num(f.aGpu) ?? 85,
      for_minutes: num(f.aFor) ?? 5,
    },
    updates: { ...s.updates, auto: f.upAuto, channel: f.upChannel },
  };
}

function General() {
  const r = useApi<InstanceSettings>("/settings", (e) => e.kind === "settings");
  return <Loaded r={r}>{(s) => <GeneralForm key={JSON.stringify(s)} settings={s} onSaved={r.set} />}</Loaded>;
}

function GeneralForm(props: { settings: InstanceSettings; onSaved: (s: InstanceSettings) => void }) {
  const [f, setF] = useState(() => toForm(props.settings));
  const set = (p: Partial<GeneralForm>) => setF((x) => ({ ...x, ...p }));
  const text = (k: keyof GeneralForm) => ({ value: f[k] as string, onInput: (e: { currentTarget: { value: string } }) => set({ [k]: e.currentTarget.value }) });
  const usage = useApi<{ total_bytes: number }>("/logs/usage");
  const changes = mergePatch(props.settings, toSettings(props.settings, f));
  const dirty = Object.keys(changes).length > 0;
  const s = useSubmit(async () => {
    props.onSaved(await patch<InstanceSettings>("/settings", changes));
    toast("Settings saved.");
  });
  return (
    <form class="space-y-6" onSubmit={s.submit}>
      <Card title="Panel">
        <div class="space-y-4">
          <Field label="Panel domain" hint="Point the domain's DNS at this server first. Tugport then serves the panel there with HTTPS, and Git hosts send webhooks to it.">
            <input class="input font-mono" placeholder="deploy.example.com" spellcheck={false} {...text("panelDomain")} />
          </Field>
          <Field label="Email for Let's Encrypt" hint="Let's Encrypt writes here about certificate problems.">
            <input class="input" type="email" {...text("acmeEmail")} />
          </Field>
          <Field label="Trusted proxies" hint="Only these addresses may set the client IP through X-Forwarded-For, like Cloudflare or a load balancer. One range per line.">
            <textarea class="input h-20" spellcheck={false} placeholder="173.245.48.0/20" {...text("trusted")} />
          </Field>
        </div>
      </Card>
      <Card title="Deploys and builds">
        <div class="space-y-4">
          <Field label="When a deploy starts while another one runs" hint="Services can override this in their own settings.">
            <Select
              value={f.policy}
              onChange={(policy) => set({ policy })}
              options={[
                ["cancel", "Cancel the running deploy"],
                ["queue", "Wait for the running deploy"],
              ]}
            />
          </Field>
          <div class="grid gap-4 sm:grid-cols-3">
            <Field label="Parallel builds">
              <input class="input" type="number" min={1} {...text("maxParallel")} />
            </Field>
            <Field label="Build CPU cap (cores)" hint="Empty means half of the machine.">
              <input class="input" type="number" min={0} step="0.5" {...text("bCpus")} />
            </Field>
            <Field label="Build memory cap (MB)" hint="Empty means half of the memory.">
              <input class="input" type="number" min={0} {...text("bMem")} />
            </Field>
          </div>
        </div>
      </Card>
      <Card title="Cleanup">
        <div class="space-y-4">
          <Check
            checked={f.dcEnabled}
            onChange={(dcEnabled) => set({ dcEnabled })}
            label="Clean up Docker automatically"
            hint="Removes unused images and caps the build cache. It keeps the images of running services and the last few for rollbacks, and it never touches volumes."
          />
          {f.dcEnabled && (
            <div class="grid gap-4 sm:grid-cols-2">
              <Field label="Every (hours)">
                <input class="input" type="number" min={1} {...text("dcHours")} />
              </Field>
              <Field label="Build cache to keep (GB)">
                <input class="input" type="number" min={0} {...text("dcCache")} />
              </Field>
            </div>
          )}
          <Check
            checked={f.lcEnabled}
            onChange={(lcEnabled) => set({ lcEnabled })}
            label="Delete old logs automatically"
            hint={usage.data ? `Logs use ${bytes(usage.data.total_bytes)} right now. The oldest are deleted first.` : "The oldest logs are deleted first once they pass the size or age below."}
          />
          {f.lcEnabled && (
            <div class="grid gap-4 sm:grid-cols-2">
              <Field label="Keep at most (GB)">
                <input class="input" type="number" min={0} step="0.5" {...text("lcGb")} />
              </Field>
              <Field label="Keep at most (days)" hint="Empty means no age limit.">
                <input class="input" type="number" min={1} {...text("lcDays")} />
              </Field>
            </div>
          )}
        </div>
      </Card>
      <Card title="Alert thresholds">
        <p class="mb-4 text-xs text-muted">Alerts go to the channels set up under Notifications once a condition lasts for the time below.</p>
        <div class="grid grid-cols-2 gap-4 sm:grid-cols-5">
          <Field label="CPU (%)">
            <input class="input" type="number" min={1} max={100} {...text("aCpu")} />
          </Field>
          <Field label="Memory (%)">
            <input class="input" type="number" min={1} max={100} {...text("aMem")} />
          </Field>
          <Field label="Disk (%)">
            <input class="input" type="number" min={1} max={100} {...text("aDisk")} />
          </Field>
          <Field label="GPU (°C)">
            <input class="input" type="number" min={1} {...text("aGpu")} />
          </Field>
          <Field label="For (minutes)">
            <input class="input" type="number" min={1} {...text("aFor")} />
          </Field>
        </div>
      </Card>
      <Card title="Updates">
        <div class="space-y-4">
          <Check checked={f.upAuto} onChange={(upAuto) => set({ upAuto })} label="Install new Tugport releases automatically" hint="Updates restart Tugport without dropping traffic." />
          <Field label="Channel">
            <Select value={f.upChannel} onChange={(upChannel) => set({ upChannel })} options={["stable", "beta"]} />
          </Field>
        </div>
      </Card>
      {dirty && (
        <div class="sticky bottom-0 z-10 -mx-4 flex flex-wrap items-center justify-between gap-2 border-t border-line bg-surface px-4 py-3 md:mx-0 md:rounded-lg md:border">
          <span class="text-dim">You have unsaved changes.</span>
          <span class="flex gap-2">
            <button type="button" class="btn" onClick={() => setF(toForm(props.settings))}>
              Discard
            </button>
            <button class="btn btn-primary" disabled={s.busy}>
              Save settings
            </button>
          </span>
          {s.error && (
            <div class="w-full">
              <ErrorBox error={s.error} />
            </div>
          )}
        </div>
      )}
    </form>
  );
}

// ---- Git ----

interface Provider {
  id: string;
  kind: string;
  name: string;
  base_url: string;
  created_at: number;
}

/** GitHub's manifest flow: the browser must POST the manifest to GitHub as a form field. */
async function startGithubApp(organization: string) {
  const r = await post<{ action: string; manifest: unknown }>("/git/github/manifest", organization ? { organization } : {});
  const form = document.createElement("form");
  form.method = "post";
  form.action = r.action;
  form.hidden = true;
  const field = document.createElement("input");
  field.type = "hidden";
  field.name = "manifest";
  field.value = JSON.stringify(r.manifest);
  form.appendChild(field);
  document.body.appendChild(form);
  form.submit();
}

function Git(props: { owner: boolean }) {
  const r = useApi<{ providers: Provider[] }>("/git/providers");
  const settings = useApi<InstanceSettings>(props.owner ? "/settings" : null);
  const [org, setOrg] = useState("");
  const [f, setF] = useState({ kind: "forgejo", name: "", base_url: "", token: "" });
  const needsBase = f.kind === "forgejo" || f.kind === "gitea";
  const s = useSubmit(async () => {
    await post("/git/providers", { kind: f.kind, name: f.name.trim() || undefined, base_url: f.base_url.trim() || undefined, token: f.token.trim() });
    setF({ kind: f.kind, name: "", base_url: "", token: "" });
    toast("Connected.");
    await r.reload();
  });
  return (
    <>
      <Card title="Connected Git hosts" pad={false}>
        <Loaded r={r}>
          {(d) =>
            d.providers.length === 0 ? (
              <div class="p-4">
                <Empty>Nothing is connected yet. Connected hosts let you pick repositories and deploy on every push.</Empty>
              </div>
            ) : (
              <ul class="divide-y divide-line">
                {d.providers.map((p) => (
                  <li key={p.id} class="flex flex-wrap items-center gap-3 px-4 py-3">
                    <div class="min-w-0 flex-1">
                      <span class="font-medium">{p.name}</span> <span class="chip">{PROVIDER_KINDS[p.kind] ?? p.kind}</span>
                      <p class="truncate text-xs text-muted">
                        {p.base_url} · connected {ago(p.created_at)}
                      </p>
                    </div>
                    {props.owner && (
                      <Action class="btn btn-ghost text-bad" confirm={`Disconnect ${p.name}?`} run={() => del(`/git/providers/${p.id}`).then(r.reload)} done="Disconnected.">
                        Disconnect
                      </Action>
                    )}
                  </li>
                ))}
              </ul>
            )
          }
        </Loaded>
      </Card>
      {props.owner && (
        <>
          <Card title="Connect GitHub">
            <div class="space-y-4">
              <p class="text-dim">
                Tugport creates a private GitHub App for this server. GitHub then sets up push webhooks by itself, and deploys report their status on each commit.
              </p>
              {settings.data && !settings.data.panel_domain && (
                <Note tone="warn">GitHub must reach this panel to deliver pushes, so set a panel domain under General before connecting.</Note>
              )}
              <Field label="Organization (optional)" hint="Leave this empty to create the app on your personal account.">
                <input class="input" placeholder="my-org" value={org} onInput={(e) => setOrg(e.currentTarget.value.trim())} />
              </Field>
              <Action class="btn btn-primary" run={() => startGithubApp(org)}>
                Connect GitHub
              </Action>
            </div>
          </Card>
          <Card title="Connect with a token">
            <form class="space-y-4" onSubmit={s.submit}>
              <p class="text-dim">Forgejo, Gitea and GitLab connect with an access token. Tugport adds the push webhook to each repository you deploy.</p>
              <div class="grid gap-4 sm:grid-cols-2">
                <Field label="Host">
                  <Select
                    value={f.kind}
                    onChange={(kind) => setF({ ...f, kind })}
                    options={[
                      ["forgejo", "Forgejo"],
                      ["gitea", "Gitea"],
                      ["gitlab", "GitLab"],
                      ["github_token", "GitHub (personal token)"],
                    ]}
                  />
                </Field>
                <Field label="Name (optional)">
                  <input class="input" value={f.name} onInput={(e) => setF({ ...f, name: e.currentTarget.value })} />
                </Field>
              </div>
              <Field label={needsBase ? "Address" : "Address (optional)"} hint={needsBase ? undefined : `Leave this empty for ${f.kind === "gitlab" ? "gitlab.com" : "github.com"}.`}>
                <input class="input font-mono" type="url" required={needsBase} placeholder="https://git.example.com" value={f.base_url} onInput={(e) => setF({ ...f, base_url: e.currentTarget.value })} />
              </Field>
              <Field label="Access token" hint="It needs read access to repositories and permission to manage their webhooks.">
                <input class="input font-mono" type="password" required autoComplete="off" value={f.token} onInput={(e) => setF({ ...f, token: e.currentTarget.value })} />
              </Field>
              <ErrorBox error={s.error} />
              <button class="btn btn-primary" disabled={s.busy}>
                {s.busy ? "Checking the token…" : "Connect"}
              </button>
            </form>
          </Card>
        </>
      )}
    </>
  );
}

// ---- Planned sections ----

interface Channel {
  id: string;
  kind: string;
  name: string;
  events: string[];
  enabled: boolean;
}

const CHANNEL_FIELDS: Record<string, [string, string, string?][]> = {
  telegram: [
    ["bot_token", "Bot token", "password"],
    ["chat_id", "Chat ID"],
    ["thread_id", "Topic ID (optional)"],
  ],
  email: [
    ["host", "SMTP host"],
    ["port", "Port (optional)", "number"],
    ["username", "User name (optional)"],
    ["password", "Password (optional)", "password"],
    ["from", "From address", "email"],
    ["to", "To address", "email"],
  ],
  webhook: [["url", "URL", "url"]],
};

function Notifications() {
  const r = useApi<{ channels: Channel[] }>("/notifications");
  const [open, setOpen] = useState(false);
  const [f, setF] = useState<{ kind: string; name: string; events: string; tls: boolean; config: Record<string, string> }>({
    kind: "telegram",
    name: "",
    events: "*",
    tls: true,
    config: {},
  });
  const s = useSubmit(async () => {
    const config: Record<string, unknown> = Object.fromEntries(Object.entries(f.config).filter(([, v]) => v.trim() !== ""));
    if (config.port) config.port = Number(config.port);
    if (f.kind === "email") config.tls = f.tls;
    await post("/notifications", { kind: f.kind, name: f.name.trim(), config, events: words(f.events) });
    setOpen(false);
    setF({ kind: f.kind, name: "", events: "*", tls: true, config: {} });
    await r.reload();
  });
  if (isPlanned(r.error)) return <Planned />;
  return (
    <Card
      title="Notification channels"
      pad={false}
      actions={
        <button class="btn" onClick={() => setOpen(true)}>
          Add channel
        </button>
      }
    >
      <Loaded r={r}>
        {(d) =>
          d.channels.length === 0 ? (
            <div class="p-4">
              <Empty>No channels yet. Tugport can report deploys, certificate problems and alerts to Telegram, email or a webhook.</Empty>
            </div>
          ) : (
            <ul class="divide-y divide-line">
              {d.channels.map((c) => (
                <li key={c.id} class="flex flex-wrap items-center gap-3 px-4 py-3">
                  <div class="min-w-0 flex-1">
                    <span class="font-medium">{c.name}</span> <span class="chip">{c.kind}</span>
                    <p class="text-xs text-muted">{c.events.includes("*") ? "All events" : c.events.join(", ")}</p>
                  </div>
                  <Action run={() => post(`/notifications/${c.id}/test`)} done="Test message sent.">
                    Send test
                  </Action>
                  <Action class="btn btn-ghost text-bad" confirm={`Remove ${c.name}?`} run={() => del(`/notifications/${c.id}`).then(r.reload)}>
                    Remove
                  </Action>
                </li>
              ))}
            </ul>
          )
        }
      </Loaded>
      <Modal open={open} onClose={() => setOpen(false)} title="Add notification channel">
        <form class="space-y-4" onSubmit={s.submit}>
          <div class="grid gap-4 sm:grid-cols-2">
            <Field label="Kind">
              <Select
                value={f.kind}
                onChange={(kind) => setF({ ...f, kind, config: {} })}
                options={[
                  ["telegram", "Telegram"],
                  ["email", "Email"],
                  ["webhook", "Webhook"],
                ]}
              />
            </Field>
            <Field label="Name">
              <input class="input" required value={f.name} onInput={(e) => setF({ ...f, name: e.currentTarget.value })} />
            </Field>
          </div>
          {(CHANNEL_FIELDS[f.kind] ?? []).map(([key, label, type]) => (
            <Field key={key} label={label}>
              <input
                class="input"
                type={type ?? "text"}
                required={!label.includes("optional")}
                autoComplete="off"
                value={f.config[key] ?? ""}
                onInput={(e) => setF({ ...f, config: { ...f.config, [key]: e.currentTarget.value } })}
              />
            </Field>
          ))}
          {f.kind === "email" && <Check checked={f.tls} onChange={(tls) => setF({ ...f, tls })} label="Use TLS" />}
          <Field label="Events" hint="Use * for everything, or list events such as deploy.failed, deploy.succeeded and certificate.failed.">
            <input class="input font-mono" value={f.events} onInput={(e) => setF({ ...f, events: e.currentTarget.value })} />
          </Field>
          <ErrorBox error={s.error} />
          <div class="flex justify-end gap-2">
            <button type="button" class="btn" onClick={() => setOpen(false)}>
              Cancel
            </button>
            <button class="btn btn-primary" disabled={s.busy}>
              Add channel
            </button>
          </div>
        </form>
      </Modal>
    </Card>
  );
}

function Registries() {
  const r = useApi<{ registries: { id: string; host: string; username: string }[] }>("/registries");
  const [f, setF] = useState({ host: "", username: "", password: "" });
  const s = useSubmit(async () => {
    await post("/registries", f);
    setF({ host: "", username: "", password: "" });
    await r.reload();
  });
  if (isPlanned(r.error)) return <Planned />;
  return (
    <>
      <Card title="Registries" pad={false}>
        <Loaded r={r}>
          {(d) =>
            d.registries.length === 0 ? (
              <div class="p-4">
                <Empty>Add a registry to deploy private images from it. Public images need nothing here.</Empty>
              </div>
            ) : (
              <ul class="divide-y divide-line">
                {d.registries.map((g) => (
                  <li key={g.id} class="flex items-center gap-3 px-4 py-3">
                    <span class="min-w-0 flex-1 truncate">
                      <span class="font-mono">{g.host}</span> <span class="text-muted">as {g.username}</span>
                    </span>
                    <Action class="btn btn-ghost text-bad" confirm={`Remove ${g.host}?`} run={() => del(`/registries/${g.id}`).then(r.reload)}>
                      Remove
                    </Action>
                  </li>
                ))}
              </ul>
            )
          }
        </Loaded>
      </Card>
      <Card title="Add a registry">
        <form class="space-y-4" onSubmit={s.submit}>
          <Field label="Registry host">
            <input class="input font-mono" required placeholder="ghcr.io" value={f.host} onInput={(e) => setF({ ...f, host: e.currentTarget.value.trim() })} />
          </Field>
          <div class="grid gap-4 sm:grid-cols-2">
            <Field label="User name">
              <input class="input" required autoComplete="off" value={f.username} onInput={(e) => setF({ ...f, username: e.currentTarget.value })} />
            </Field>
            <Field label="Password or token">
              <input class="input" type="password" required autoComplete="off" value={f.password} onInput={(e) => setF({ ...f, password: e.currentTarget.value })} />
            </Field>
          </div>
          <ErrorBox error={s.error} />
          <button class="btn btn-primary" disabled={s.busy}>
            Add registry
          </button>
        </form>
      </Card>
    </>
  );
}

interface Target {
  id: string;
  name: string;
  kind: string;
}
interface Job {
  id: string;
  service_id: string | null;
  target_id: string;
  kind: string;
  volume?: string | null;
  schedule: string;
  keep_last: number;
}
interface Run {
  id: string;
  kind: string;
  status: string;
  snapshot: string | null;
  size_bytes: number | null;
  error: string | null;
  started_at: number;
  finished_at: number | null;
}

function Backups() {
  const targets = useApi<{ targets: Target[] }>("/backups/targets");
  const jobs = useApi<{ jobs: Job[] }>("/backups/jobs");
  const [addTarget, setAddTarget] = useState(false);
  const [addJob, setAddJob] = useState(false);
  const [runsOf, setRunsOf] = useState<Job | null>(null);
  if (isPlanned(targets.error) || isPlanned(jobs.error)) return <Planned />;
  const targetName = (id: string) => targets.data?.targets.find((t) => t.id === id)?.name ?? id;
  return (
    <>
      <Card
        title="Backup targets"
        pad={false}
        actions={
          <button class="btn" onClick={() => setAddTarget(true)}>
            Add target
          </button>
        }
      >
        <Loaded r={targets}>
          {(d) =>
            d.targets.length === 0 ? (
              <div class="p-4">
                <Empty>Backups go to S3-compatible storage or a folder on this server, so add one of those first.</Empty>
              </div>
            ) : (
              <ul class="divide-y divide-line">
                {d.targets.map((t) => (
                  <li key={t.id} class="flex items-center gap-3 px-4 py-3">
                    <span class="min-w-0 flex-1">
                      <span class="font-medium">{t.name}</span> <span class="chip">{t.kind}</span>
                    </span>
                    <Action class="btn btn-ghost text-bad" confirm={`Remove ${t.name}?`} run={() => del(`/backups/targets/${t.id}`).then(targets.reload)}>
                      Remove
                    </Action>
                  </li>
                ))}
              </ul>
            )
          }
        </Loaded>
      </Card>
      <Card
        title="Backup jobs"
        pad={false}
        actions={
          <button class="btn" disabled={!targets.data?.targets.length} onClick={() => setAddJob(true)}>
            Add job
          </button>
        }
      >
        <Loaded r={jobs}>
          {(d) =>
            d.jobs.length === 0 ? (
              <div class="p-4">
                <Empty>No scheduled backups yet. Jobs back up databases, volumes or Tugport's own state on a schedule.</Empty>
              </div>
            ) : (
              <ul class="divide-y divide-line">
                {d.jobs.map((j) => (
                  <li key={j.id} class="flex flex-wrap items-center gap-3 px-4 py-3">
                    <div class="min-w-0 flex-1">
                      <span class="font-medium">{j.kind === "state" ? "Tugport state" : `${j.kind}${j.volume ? ` ${j.volume}` : ""}`}</span>
                      <p class="text-xs text-muted">
                        <code>{j.schedule}</code> to {targetName(j.target_id)}, keeping the last {j.keep_last}
                      </p>
                    </div>
                    <button class="btn" onClick={() => setRunsOf(j)}>
                      Runs
                    </button>
                    <Action run={() => post(`/backups/jobs/${j.id}/run`)} done="Backup started.">
                      Run now
                    </Action>
                    <Action class="btn btn-ghost text-bad" confirm="Remove this job? Existing backups stay in the target." run={() => del(`/backups/jobs/${j.id}`).then(jobs.reload)}>
                      Remove
                    </Action>
                  </li>
                ))}
              </ul>
            )
          }
        </Loaded>
      </Card>
      {addTarget && <TargetForm onClose={() => setAddTarget(false)} onSaved={targets.reload} />}
      {addJob && targets.data && <JobForm targets={targets.data.targets} onClose={() => setAddJob(false)} onSaved={jobs.reload} />}
      {runsOf && <Runs job={runsOf} onClose={() => setRunsOf(null)} />}
    </>
  );
}

function TargetForm(props: { onClose: () => void; onSaved: () => void }) {
  const [f, setF] = useState<Record<string, string>>({ name: "", kind: "s3" });
  const s3: [string, string, string?][] = [
    ["endpoint", "Endpoint", "url"],
    ["bucket", "Bucket"],
    ["prefix", "Prefix (optional)"],
    ["region", "Region (optional)"],
    ["access_key", "Access key"],
    ["secret_key", "Secret key", "password"],
  ];
  const s = useSubmit(async () => {
    const fields = f.kind === "s3" ? s3.map(([k]) => k) : ["path"];
    const config = Object.fromEntries(fields.filter((k) => f[k]?.trim()).map((k) => [k, f[k].trim()]));
    await post("/backups/targets", { name: f.name.trim(), kind: f.kind, config });
    props.onSaved();
    props.onClose();
  });
  const input = (k: string, label: string, type = "text") => (
    <Field key={k} label={label}>
      <input class="input" type={type} required={!label.includes("optional")} autoComplete="off" value={f[k] ?? ""} onInput={(e) => setF({ ...f, [k]: e.currentTarget.value })} />
    </Field>
  );
  return (
    <Modal open onClose={props.onClose} title="Add backup target">
      <form class="space-y-4" onSubmit={s.submit}>
        <div class="grid gap-4 sm:grid-cols-2">
          {input("name", "Name")}
          <Field label="Kind">
            <Select
              value={f.kind}
              onChange={(kind) => setF({ ...f, kind })}
              options={[
                ["s3", "S3-compatible storage"],
                ["local", "Folder on this server"],
              ]}
            />
          </Field>
        </div>
        {f.kind === "s3" ? s3.map(([k, label, type]) => input(k, label, type)) : input("path", "Folder")}
        <ErrorBox error={s.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={s.busy}>
            Add target
          </button>
        </div>
      </form>
    </Modal>
  );
}

function JobForm(props: { targets: Target[]; onClose: () => void; onSaved: () => void }) {
  const projects = useApi<{ projects: Project[] }>("/projects");
  const [services, setServices] = useState<[string, string][] | null>(null);
  const [f, setF] = useState({ kind: "database", service: "", volume: "", target: props.targets[0]?.id ?? "", schedule: "0 3 * * *", keep: "7" });
  // ponytail: one request per project to list services; a flat GET /api/services would replace this.
  useEffect(() => {
    if (!projects.data) return;
    Promise.all(projects.data.projects.map((p) => get<ProjectDetail>(`/projects/${p.id}`)))
      .then((all) => setServices(all.flatMap((d) => d.services.filter((x) => x.kind !== "compose").map((x): [string, string] => [x.id, `${d.project.name}.${x.name}`]))))
      .catch(() => setServices([]));
  }, [projects.data]);
  const s = useSubmit(async () => {
    await post("/backups/jobs", {
      kind: f.kind,
      service_id: f.kind === "state" ? undefined : f.service,
      volume: f.kind === "volume" ? f.volume.trim() : undefined,
      target_id: f.target,
      schedule: f.schedule.trim(),
      keep_last: num(f.keep) ?? 7,
    });
    props.onSaved();
    props.onClose();
  });
  return (
    <Modal open onClose={props.onClose} title="Add backup job">
      <form class="space-y-4" onSubmit={s.submit}>
        <Field label="What to back up">
          <Select
            value={f.kind}
            onChange={(kind) => setF({ ...f, kind })}
            options={[
              ["database", "A database (consistent dump)"],
              ["volume", "A volume"],
              ["state", "Tugport's own state"],
            ]}
          />
        </Field>
        {f.kind !== "state" && (
          <Field label="Service">
            <Select value={f.service} onChange={(service) => setF({ ...f, service })} options={[["", services ? "Choose…" : "Loading…"], ...(services ?? [])]} />
          </Field>
        )}
        {f.kind === "volume" && (
          <Field label="Volume name">
            <input class="input font-mono" required value={f.volume} onInput={(e) => setF({ ...f, volume: e.currentTarget.value })} />
          </Field>
        )}
        <div class="grid gap-4 sm:grid-cols-3">
          <Field label="Target">
            <Select value={f.target} onChange={(target) => setF({ ...f, target })} options={props.targets.map((t): [string, string] => [t.id, t.name])} />
          </Field>
          <Field label="Schedule" hint="Cron format in the server's time zone.">
            <input class="input font-mono" required value={f.schedule} onInput={(e) => setF({ ...f, schedule: e.currentTarget.value })} />
          </Field>
          <Field label="Keep the last">
            <input class="input" type="number" min={1} value={f.keep} onInput={(e) => setF({ ...f, keep: e.currentTarget.value })} />
          </Field>
        </div>
        <ErrorBox error={s.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={s.busy || (f.kind !== "state" && !f.service)}>
            Add job
          </button>
        </div>
      </form>
    </Modal>
  );
}

function Runs(props: { job: Job; onClose: () => void }) {
  const r = useApi<{ runs: Run[] }>(`/backups/jobs/${props.job.id}/runs`);
  return (
    <Modal open onClose={props.onClose} title="Backup runs" wide>
      <Loaded r={r} planned>
        {(d) =>
          d.runs.length === 0 ? (
            <Empty>This job has not run yet.</Empty>
          ) : (
            <ul class="divide-y divide-line rounded-md border border-line">
              {d.runs.map((run) => (
                <li key={run.id} class="flex flex-wrap items-center gap-3 px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <Pill state={run.status} />
                      <span class="text-dim">{when(run.started_at)}</span>
                      {run.size_bytes != null && <span class="text-muted">{bytes(run.size_bytes)}</span>}
                    </div>
                    {run.error && <p class="text-bad">{run.error}</p>}
                  </div>
                  {run.snapshot && (
                    <>
                      <Action run={() => post(`/backups/runs/${run.id}/restore`, { test: true })} done="Test restore started; the result arrives as a notification.">
                        Test restore
                      </Action>
                      <Action class="btn btn-danger" confirm="Restore this backup? It replaces the current data." run={() => post(`/backups/runs/${run.id}/restore`, {})} done="Restore started.">
                        Restore
                      </Action>
                    </>
                  )}
                </li>
              ))}
            </ul>
          )
        }
      </Loaded>
    </Modal>
  );
}

function Tunnel() {
  const r = useApi<{ enabled: boolean; account?: string; tunnel_id?: string; status?: string }>("/tunnel");
  const [token, setToken] = useState("");
  const s = useSubmit(async () => {
    await put("/tunnel", { api_token: token.trim() });
    setToken("");
    await r.reload();
  });
  return (
    <Card title="Cloudflare Tunnel">
      <Loaded r={r} planned>
        {(t) =>
          t.enabled ? (
            <div class="space-y-3">
              <p class="flex flex-wrap items-center gap-2">
                <Pill state={t.status ?? "running"} /> <span class="text-dim">Tunnel {t.tunnel_id}</span>
                {t.account && <span class="text-muted">on {t.account}</span>}
              </p>
              <p class="text-dim">Domains with the tunnel switch on reach this server through Cloudflare, so the server needs no open ports for them.</p>
              <Action class="btn btn-danger" confirm="Disconnect the tunnel? Domains routed through it stop working until you connect again." run={() => del("/tunnel").then(r.reload)}>
                Disconnect
              </Action>
            </div>
          ) : (
            <form class="space-y-4" onSubmit={s.submit}>
              <p class="text-dim">
                With an API token, Tugport creates a tunnel, runs cloudflared and adds the DNS records for every domain that uses the tunnel.
              </p>
              <Field label="Cloudflare API token" hint="It needs the Cloudflare Tunnel edit and DNS edit permissions for your zones.">
                <input class="input font-mono" type="password" required autoComplete="off" value={token} onInput={(e) => setToken(e.currentTarget.value)} />
              </Field>
              <ErrorBox error={s.error} />
              <button class="btn btn-primary" disabled={s.busy}>
                Connect
              </button>
            </form>
          )
        }
      </Loaded>
    </Card>
  );
}

function About() {
  const r = useApi<{ version: string; commit: string; source: string; docker: string | null }>("/about");
  return (
    <Card title="About Tugport">
      <Loaded r={r}>
        {(a) => (
          <dl class="grid grid-cols-[8rem_1fr] gap-y-2">
            <Row label="Version">{a.version}</Row>
            <Row label="Commit">
              <code>{a.commit}</code>
            </Row>
            <Row label="Docker">{a.docker ?? "not reachable"}</Row>
            <Row label="Source code">
              <a class="link break-all" href={a.source} target="_blank" rel="noreferrer">
                {a.source}
              </a>
            </Row>
          </dl>
        )}
      </Loaded>
    </Card>
  );
}

function Row(props: { label: string; children: ComponentChildren }) {
  return (
    <>
      <dt class="text-dim">{props.label}</dt>
      <dd class="min-w-0">{props.children}</dd>
    </>
  );
}
