import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { del, patch, post, put, useApi, useMe, useNodes } from "../api";
import { Icon } from "../icons";
import {
  Action,
  Card,
  CardSkeleton,
  Check,
  Empty,
  ErrorBox,
  Field,
  FormButtons,
  IconButton,
  Input,
  Loaded,
  Modal,
  Note,
  Page,
  Pill,
  Rows,
  RowsSkeleton,
  Select,
  Tabs,
  TextArea,
  UnsavedBar,
  toast,
  useForm,
  useSubmit,
} from "../ui";
import { ago, bytes, cidr, mergePatch, num, sentence, str, when, words } from "../util";
import { Progress, RunLog, type RunProgress } from "./actions";
import { PROVIDER_KINDS } from "./new-service";

const TABS: [string, string][] = [
  ["general", "General"],
  ["git", "Git"],
  ["notifications", "Notifications"],
  ["registries", "Registries"],
  ["backups", "Backups"],
  ["tunnel", "Cloudflare Tunnel"],
  ["about", "About"],
];

export function Settings(props: { tab?: string }) {
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

function toForm(s: InstanceSettings) {
  return {
    panelDomain: s.panel_domain ?? "",
    panelPublic: s.panel_public ?? true,
    panelListen: s.panel_listen ?? "0.0.0.0:8484",
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
    panel_public: f.panelPublic,
    panel_listen: f.panelListen.trim() || "0.0.0.0:8484",
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
  const skeleton = (
    <div class="space-y-6">
      <CardSkeleton />
      <CardSkeleton />
      <CardSkeleton />
    </div>
  );
  return (
    <Loaded r={r} skeleton={skeleton}>
      {(s) => <GeneralForm key={JSON.stringify(s)} settings={s} onSaved={r.set} />}
    </Loaded>
  );
}

function GeneralForm(props: { settings: InstanceSettings; onSaved: (s: InstanceSettings) => void }) {
  const { f, set, bind, reset } = useForm(toForm(props.settings));
  const usage = useApi<{ total_bytes: number }>("/logs/usage");
  const channels = useApi<{ channels: { name: string; kind: string; events: string[] }[] }>("/notifications");
  // Builds default to half of the panel's server; show what that is.
  const local = useNodes()?.nodes.find((n) => n.local)?.info;
  const halfCpus = local?.cpus ? local.cpus / 2 : null;
  const halfMb = local?.memory_total ? Math.floor(local.memory_total / 2 / 1048576) : null;
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
          <Input
            label="Panel domain"
            info="Point the domain's DNS at this server first. Tugport then serves the panel there with HTTPS, and Git hosts send webhooks to it."
            mono
            placeholder="deploy.example.com"
            {...bind("panelDomain")}
          />
          {"panel_public" in props.settings && (
            <Check
              checked={f.panelPublic}
              onChange={(panelPublic) => set({ panelPublic })}
              label="Serve the panel on the panel domain"
              info="Turn this off to keep the panel private. The panel domain then answers only Git and deploy webhooks, and you open the panel through the address below, such as a tailnet IP."
            />
          )}
          <Input
            label="Panel address"
            info="The address and port the panel itself listens on, like 100.64.0.5:8484 for a tailnet address. A change takes effect when Tugport restarts, so make sure you can reach the new address first."
            mono
            {...bind("panelListen")}
          />
          <Input label="Email for Let's Encrypt" info="Let's Encrypt writes here about certificate problems." type="email" {...bind("acmeEmail")} />
          <TextArea
            label="Trusted proxies"
            info="Only these addresses may set the client IP through X-Forwarded-For, like Cloudflare or a load balancer; write one range per line."
            class="h-20"
            placeholder="173.245.48.0/20"
            {...bind("trusted")}
          />
        </div>
      </Card>
      <Card title="Deploys and builds">
        <div class="space-y-4">
          <Field
            label="When a deploy starts while another one runs"
            info="Cancelling suits frequent pushes, since only the newest one matters. Waiting makes every deploy finish in order. Services can override this in their own settings."
          >
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
            <Input label="Parallel builds" info="How many builds may run at once; more wait in a queue." type="number" min={1} {...bind("maxParallel")} />
            <Input
              label="Build CPU cap (cores)"
              info="The most CPU one build may use, so a build never starves the running services. Leave it empty for half of the server."
              hint={halfCpus ? `Empty means half of the server: ${halfCpus} cores.` : "Empty means half of the server."}
              placeholder={halfCpus ? String(halfCpus) : undefined}
              type="number"
              min={0}
              step="0.5"
              {...bind("bCpus")}
            />
            <Input
              label="Build memory cap (MB)"
              info="The most memory one build may use. A build that goes over fails instead of pushing running services out of memory. Leave it empty for half of the server."
              hint={halfMb ? `Empty means half of the server: ${halfMb.toLocaleString("en")} MB.` : "Empty means half of the server."}
              placeholder={halfMb ? String(halfMb) : undefined}
              type="number"
              min={0}
              {...bind("bMem")}
            />
          </div>
        </div>
      </Card>
      <Card title="Cleanup">
        <div class="space-y-4">
          <Check
            checked={f.dcEnabled}
            onChange={(dcEnabled) => set({ dcEnabled })}
            label="Clean up Docker automatically"
            info="Tugport removes unused images and caps the build cache, but it keeps the images of running services and the last few for rollbacks, and it never touches volumes."
          />
          {f.dcEnabled && (
            <div class="grid gap-4 sm:grid-cols-2">
              <Input label="Every (hours)" info="How often the cleanup runs." type="number" min={1} {...bind("dcHours")} />
              <Input label="Build cache to keep (GB)" info="Cached build layers make rebuilds fast. The cleanup trims the cache down to this size, oldest layers first." type="number" min={0} {...bind("dcCache")} />
            </div>
          )}
          <Check
            checked={f.lcEnabled}
            onChange={(lcEnabled) => set({ lcEnabled })}
            label="Delete old logs automatically"
            hint={
              usage.data
                ? `Logs use ${bytes(usage.data.total_bytes)} right now. The oldest are deleted first.`
                : "The oldest logs are deleted first once they pass the size or age below."
            }
          />
          {f.lcEnabled && (
            <div class="grid gap-4 sm:grid-cols-2">
              <Input label="Keep at most (GB)" info="The total size of all stored logs." type="number" min={0} step="0.5" {...bind("lcGb")} />
              <Input label="Keep at most (days)" info="Empty means no age limit." type="number" min={1} {...bind("lcDays")} />
            </div>
          )}
        </div>
      </Card>
      <Card title="Alert thresholds">
        <AlertDestinations channels={channels.data?.channels} />
        <div class="grid grid-cols-2 gap-4 sm:grid-cols-5">
          <Input label="CPU (%)" info="An alert goes out when a server's CPU use stays above this." type="number" min={1} max={100} {...bind("aCpu")} />
          <Input label="Memory (%)" info="An alert goes out when a server's memory use stays above this." type="number" min={1} max={100} {...bind("aMem")} />
          <Input label="Disk (%)" info="An alert goes out when any of a server's disks is fuller than this." type="number" min={1} max={100} {...bind("aDisk")} />
          <Input label="GPU (°C)" info="An alert goes out when a GPU stays hotter than this." type="number" min={1} {...bind("aGpu")} />
          <Input label="For (minutes)" info="A condition has to last this long before it alerts, so short spikes stay quiet." type="number" min={1} {...bind("aFor")} />
        </div>
      </Card>
      <Card title="Updates">
        <div class="space-y-4">
          <Check
            checked={f.upAuto}
            onChange={(upAuto) => set({ upAuto })}
            label="Install new Tugport releases automatically"
            info="Tugport checks for a new release every 6 hours and switches to it without dropping traffic."
          />
          <Field label="Channel" info="Stable gets tested releases. Beta gets new releases first and may have rough edges.">
            <Select value={f.upChannel} onChange={(upChannel) => set({ upChannel })} options={["stable", "beta"]} />
          </Field>
        </div>
      </Card>
      {dirty && <UnsavedBar s={s} onDiscard={reset} />}
    </form>
  );
}

/** Where alerts go: the channels that take them, or a warning that nothing does. */
function AlertDestinations(props: { channels?: { name: string; kind: string; events: string[] }[] }) {
  if (!props.channels) return <p class="mb-4 text-xs text-muted">Alerts go to the notification channels that take them once a condition lasts for the time below.</p>;
  const taking = props.channels.filter((c) => c.events.includes("*") || c.events.some((e) => e.startsWith("alert")));
  return taking.length ? (
    <p class="mb-4 text-dim">
      Alerts go to {taking.map((c) => `${c.name} (${c.kind})`).join(", ")} once a condition lasts for the time below.{" "}
      <a class="link" href="/settings/notifications">
        Change channels
      </a>
    </p>
  ) : (
    <Note tone="warn" class="mb-4">
      No notification channel takes alerts yet, so nobody hears about them.{" "}
      <a class="link" href="/settings/notifications">
        Add a Telegram, email or webhook channel
      </a>{" "}
      to get them.
    </Note>
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

/** What a token must allow on each host: listing repositories, managing their webhooks, posting commit statuses. */
const FORGEJO_SCOPES = "Give it access to all repositories with repository: Read and write, user: Read and organization: Read.";
const TOKEN_SCOPES: Record<string, string> = {
  forgejo: FORGEJO_SCOPES,
  gitea: FORGEJO_SCOPES,
  gitlab: "Give it the api scope.",
  github_token: "Give it the repo and admin:repo_hook scopes.",
};

/** Rotate a connection's token in place: services, webhooks and the webhook secret stay as they are. */
function ReplaceToken(props: { p: Provider; onClose: () => void; onSaved: () => void }) {
  const [token, setToken] = useState("");
  const s = useSubmit(async () => {
    await put(`/git/providers/${props.p.id}/token`, { token: token.trim() });
    toast("Token replaced.");
    props.onSaved();
    props.onClose();
  });
  return (
    <Modal open onClose={props.onClose} title={`Replace the token of ${props.p.name}`}>
      <form class="space-y-4" onSubmit={s.submit}>
        <p class="text-dim">Services deploying from this connection and its push webhooks keep working. Tugport checks the new token before saving it.</p>
        <Input
          label="New token"
          type="password"
          autocomplete="off"
          required
          hint={TOKEN_SCOPES[props.p.kind] ?? TOKEN_SCOPES.github_token}
          value={token}
          onInput={(e) => setToken(e.currentTarget.value)}
        />
        <FormButtons s={s} submit="Replace token" onCancel={props.onClose} disabled={!token.trim()} />
      </form>
    </Modal>
  );
}

function Git(props: { owner: boolean }) {
  const r = useApi<{ providers: Provider[] }>("/git/providers");
  const settings = useApi<InstanceSettings>(props.owner ? "/settings" : null);
  const [org, setOrg] = useState("");
  const [rotating, setRotating] = useState<Provider | null>(null);
  const { f, set, bind } = useForm({ kind: "forgejo", name: "", base_url: "", token: "" });
  const needsBase = f.kind === "forgejo" || f.kind === "gitea";
  const s = useSubmit(async () => {
    await post("/git/providers", { kind: f.kind, name: f.name.trim() || undefined, base_url: f.base_url.trim() || undefined, token: f.token.trim() });
    set({ name: "", base_url: "", token: "" });
    toast("Connected.");
    await r.reload();
  });
  return (
    <>
      <Card title="Connected Git hosts" pad={false}>
        <Loaded r={r} skeleton={<RowsSkeleton />}>
          {(d) => (
            <Rows items={d.providers} empty="Nothing is connected yet. Connected hosts let you pick repositories and deploy on every push.">
              {(p) => (
                <li key={p.id} class="row">
                  <div class="min-w-0 flex-1">
                    <span class="font-medium">{p.name}</span> <span class="chip">{PROVIDER_KINDS[p.kind] ?? p.kind}</span>
                    <p class="truncate text-xs text-muted">
                      {p.base_url} · connected {ago(p.created_at)}
                    </p>
                  </div>
                  {props.owner && p.kind !== "github_app" && <IconButton icon="key" label={`Replace the token of ${p.name}`} onClick={() => setRotating(p)} />}
                  {props.owner && (
                    <Action class="btn btn-ghost btn-sm px-2 text-bad" label={`Disconnect ${p.name}`} confirm={`Disconnect ${p.name}?`} run={() => del(`/git/providers/${p.id}`).then(r.reload)} done="Disconnected.">
                      <Icon name="trash" />
                    </Action>
                  )}
                </li>
              )}
            </Rows>
          )}
        </Loaded>
      </Card>
      {rotating && <ReplaceToken p={rotating} onClose={() => setRotating(null)} onSaved={r.reload} />}
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
              <Input
                label="Organization (optional)"
                hint="Leave this empty to create the app on your personal account."
                placeholder="my-org"
                value={org}
                onInput={(e) => setOrg(e.currentTarget.value.trim())}
              />
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
                    onChange={(kind) => set({ kind })}
                    options={[
                      ["forgejo", "Forgejo"],
                      ["gitea", "Gitea"],
                      ["gitlab", "GitLab"],
                      ["github_token", "GitHub (personal token)"],
                    ]}
                  />
                </Field>
                <Input label="Name (optional)" {...bind("name")} />
              </div>
              <Input
                label={needsBase ? "Address" : "Address (optional)"}
                hint={needsBase ? undefined : `Leave this empty for ${f.kind === "gitlab" ? "gitlab.com" : "github.com"}.`}
                class="font-mono"
                type="url"
                required={needsBase}
                placeholder="https://git.example.com"
                {...bind("base_url")}
              />
              <Input
                label="Access token"
                hint={TOKEN_SCOPES[f.kind] ?? TOKEN_SCOPES.github_token}
                class="font-mono"
                type="password"
                required
                autoComplete="off"
                {...bind("token")}
              />
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

// ---- Notifications and registries ----

/** One setting of a channel or a backup target; labels that say "optional" mark the optional ones. */
function ConfigInput(props: { label: string; type?: string; value: string | undefined; onChange: (v: string) => void }) {
  return (
    <Input
      label={props.label}
      type={props.type ?? "text"}
      required={!props.label.includes("optional")}
      autoComplete="off"
      value={props.value ?? ""}
      onInput={(e) => props.onChange(e.currentTarget.value)}
    />
  );
}

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
  const { f, set, bind } = useForm({ kind: "telegram", name: "", events: "*", tls: true, config: {} as Record<string, string> });
  const s = useSubmit(async () => {
    const config: Record<string, unknown> = Object.fromEntries(Object.entries(f.config).filter(([, v]) => v.trim() !== ""));
    if (config.port) config.port = Number(config.port);
    if (f.kind === "email") config.tls = f.tls;
    await post("/notifications", { kind: f.kind, name: f.name.trim(), config, events: words(f.events) });
    setOpen(false);
    set({ name: "", events: "*", tls: true, config: {} });
    await r.reload();
  });
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
        {(d) => (
          <Rows items={d.channels} empty="No channels yet. Tugport can report deploys, certificate problems and alerts to Telegram, email or a webhook.">
            {(c) => (
              <li key={c.id} class="row">
                <div class="min-w-0 flex-1">
                  <span class="font-medium">{c.name}</span> <span class="chip">{c.kind}</span>
                  <p class="text-xs text-muted">{c.events.includes("*") ? "All events" : c.events.join(", ")}</p>
                </div>
                <div class="flex flex-wrap items-center gap-2">
                  <Action run={() => post(`/notifications/${c.id}/test`)} done="Test message sent.">
                    Send test
                  </Action>
                  <Action class="btn btn-ghost btn-sm px-2 text-bad" label={`Remove ${c.name}`} confirm={`Remove ${c.name}?`} run={() => del(`/notifications/${c.id}`).then(r.reload)}>
                    <Icon name="trash" />
                  </Action>
                </div>
              </li>
            )}
          </Rows>
        )}
      </Loaded>
      <Modal open={open} onClose={() => setOpen(false)} title="Add notification channel">
        <form class="space-y-4" onSubmit={s.submit}>
          <div class="grid gap-4 sm:grid-cols-2">
            <Field label="Kind">
              <Select
                value={f.kind}
                onChange={(kind) => set({ kind, config: {} })}
                options={[
                  ["telegram", "Telegram"],
                  ["email", "Email"],
                  ["webhook", "Webhook"],
                ]}
              />
            </Field>
            <Input label="Name" required {...bind("name")} />
          </div>
          {(CHANNEL_FIELDS[f.kind] ?? []).map(([key, label, type]) => (
            <ConfigInput key={key} label={label} type={type} value={f.config[key]} onChange={(v) => set((s) => ({ config: { ...s.config, [key]: v } }))} />
          ))}
          {f.kind === "email" && <Check checked={f.tls} onChange={(tls) => set({ tls })} label="Use TLS" />}
          <Input label="Events" hint="Use * for everything, or list events such as deploy.failed, deploy.succeeded, certificate.failed and alert (server CPU, memory, disk and GPU alerts)." class="font-mono" {...bind("events")} />
          <FormButtons s={s} submit="Add channel" onCancel={() => setOpen(false)} />
        </form>
      </Modal>
    </Card>
  );
}

function Registries() {
  const r = useApi<{ registries: { id: string; host: string; username: string }[] }>("/registries");
  const { f, set, bind, reset } = useForm({ host: "", username: "", password: "" });
  const s = useSubmit(async () => {
    await post("/registries", f);
    reset();
    await r.reload();
  });
  return (
    <>
      <Card title="Registries" pad={false}>
        <Loaded r={r} skeleton={<RowsSkeleton />}>
          {(d) => (
            <Rows items={d.registries} empty="Add a registry to deploy private images from it. Public images need nothing here.">
              {(g) => (
                <li key={g.id} class="row flex-nowrap">
                  <span class="min-w-0 flex-1 truncate">
                    <span class="font-mono">{g.host}</span> <span class="text-muted">as {g.username}</span>
                  </span>
                  <Action class="btn btn-ghost btn-sm px-2 text-bad" label={`Remove ${g.host}`} confirm={`Remove ${g.host}?`} run={() => del(`/registries/${g.id}`).then(r.reload)}>
                    <Icon name="trash" />
                  </Action>
                </li>
              )}
            </Rows>
          )}
        </Loaded>
      </Card>
      <Card title="Add a registry">
        <form class="space-y-4" onSubmit={s.submit}>
          <Input label="Registry host" class="font-mono" required placeholder="ghcr.io" value={f.host} onInput={(e) => set({ host: e.currentTarget.value.trim() })} />
          <div class="grid gap-4 sm:grid-cols-2">
            <Input label="User name" required autoComplete="off" {...bind("username")} />
            <Input label="Password or token" type="password" required autoComplete="off" {...bind("password")} />
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
  storage_class?: string | null;
}

const storageClasses: [string, string][] = [
  ["STANDARD", "Standard"],
  ["STANDARD_IA", "Standard-IA"],
  ["GLACIER_IR", "Glacier Instant Retrieval"],
  ["DEEP_ARCHIVE", "Glacier Deep Archive"],
];
const storageHints: Record<string, string> = {
  STANDARD: "Standard costs the most to store, but reads are instant and there is no retrieval fee or minimum storage period.",
  STANDARD_IA: "Standard-IA costs less to store and is still instant to read, but every gigabyte read has a retrieval fee and every file is billed for at least 30 days.",
  GLACIER_IR:
    "Glacier Instant Retrieval costs less still and is instant to read, but its retrieval fee is higher and every file is billed for at least 90 days. Not every S3-compatible provider offers it.",
  DEEP_ARCHIVE:
    "Deep Archive is by far the cheapest. Restores wait up to 48 hours while S3 thaws the data, every file is billed for at least 180 days, quick test restores check only the index and not the data itself (a full test thaws it, for a retrieval fee), and the access key needs the s3:RestoreObject permission. Choose it here instead of adding a bucket lifecycle rule, because a rule would also archive the files that backups read and break them.",
};

interface Job {
  id: string;
  service_id: string | null;
  target_id: string;
  kind: string;
  volume?: string | null;
  schedule: string;
  keep_last: number;
  enabled?: boolean;
  last_run?: { id: string; status: string; finished_at: number | null; error: string | null; progress?: RunProgress | null } | null;
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
  progress?: RunProgress | null;
}

function Backups() {
  const targets = useApi<{ targets: Target[] }>("/backups/targets");
  const jobs = useApi<{ jobs: Job[] }>("/backups/jobs", (e) => e.kind === "backup");
  const [addTarget, setAddTarget] = useState(false);
  const [logId, setLogId] = useState<string | null>(null);
  const [addJob, setAddJob] = useState(false);
  const [runsOf, setRunsOf] = useState<Job | null>(null);
  const services = useServices();
  const target = (id: string) => targets.data?.targets.find((t) => t.id === id);
  const targetName = (id: string) => target(id)?.name ?? id;
  const serviceName = (id: string | null) => {
    const s = services?.find((x) => x.id === id);
    return s ? `${s.project}.${s.name}` : "a deleted service";
  };
  const what = (j: Job) =>
    j.kind === "state"
      ? "Tugport's own state"
      : j.kind === "database"
        ? `Database dump of ${serviceName(j.service_id)}`
        : `${j.volume?.startsWith("/") ? "Folder" : "Volume"} ${j.volume ?? ""} of ${serviceName(j.service_id)}`;
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
          {(d) => (
            <Rows items={d.targets} empty="Backups go to S3-compatible storage or a folder on this server, so add one of those first.">
              {(t) => (
                <li key={t.id} class="row flex-nowrap">
                  <span class="min-w-0 flex-1">
                    <span class="font-medium">{t.name}</span> <span class="chip">{t.kind}</span>{" "}
                    {t.storage_class && <span class="chip">{storageClasses.find(([c]) => c === t.storage_class)?.[1] ?? t.storage_class}</span>}
                  </span>
                  <Action class="btn btn-ghost btn-sm px-2 text-bad" label={`Remove ${t.name}`} confirm={`Remove ${t.name}?`} run={() => del(`/backups/targets/${t.id}`).then(targets.reload)}>
                    <Icon name="trash" />
                  </Action>
                </li>
              )}
            </Rows>
          )}
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
          {(d) => (
            <Rows items={d.jobs} empty="No scheduled backups yet. Jobs back up databases, volumes, host folders or Tugport's own state on a schedule.">
              {(j) => (
                <li key={j.id} class="row">
                  <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <span class="font-medium">{what(j)}</span>
                      {j.enabled === false && <span class="chip">paused</span>}
                      {j.last_run && <Pill state={j.last_run.status} />}
                    </div>
                    <p class="text-xs text-muted">
                      <code>{j.schedule}</code> to {targetName(j.target_id)}, keeping the last {j.keep_last}
                      {j.last_run?.finished_at ? `, last finished ${ago(j.last_run.finished_at)}` : ""}
                    </p>
                    {j.last_run?.status === "running" && <Progress id={j.last_run.id} initial={j.last_run.progress} class="mt-1" />}
                    {j.last_run?.error && <p class="text-xs text-bad">{j.last_run.error}</p>}
                  </div>
                  <div class="flex flex-wrap items-center gap-2">
                    {j.last_run?.status === "running" && (
                      <button class="btn" onClick={() => setLogId(j.last_run!.id)}>
                        Log
                      </button>
                    )}
                    <button class="btn" onClick={() => setRunsOf(j)}>
                      Runs
                    </button>
                    <Action run={() => patch(`/backups/jobs/${j.id}`, { enabled: j.enabled === false }).then(jobs.reload)}>{j.enabled === false ? "Resume" : "Pause"}</Action>
                    <Action run={() => post(`/backups/jobs/${j.id}/run`).then(jobs.reload)} done="Backup started.">
                      Run now
                    </Action>
                    <Action
                      class="btn btn-ghost btn-sm px-2 text-bad"
                      label="Remove this job"
                      confirm="Remove this job? Existing backups stay in the target."
                      run={() => del(`/backups/jobs/${j.id}`).then(jobs.reload)}
                    >
                      <Icon name="trash" />
                    </Action>
                  </div>
                </li>
              )}
            </Rows>
          )}
        </Loaded>
      </Card>
      {addTarget && <TargetForm onClose={() => setAddTarget(false)} onSaved={targets.reload} />}
      {addJob && targets.data && <JobForm targets={targets.data.targets} services={services} onClose={() => setAddJob(false)} onSaved={jobs.reload} />}
      {runsOf && <Runs job={runsOf} archived={target(runsOf.target_id)?.storage_class === "DEEP_ARCHIVE"} onLog={setLogId} onClose={() => setRunsOf(null)} />}
      <RunLog id={logId} onClose={() => setLogId(null)} />
    </>
  );
}

function TargetForm(props: { onClose: () => void; onSaved: () => void }) {
  const { f, set } = useForm<Record<string, string>>({ name: "", kind: "s3", storage_class: "STANDARD" });
  const s3: [string, string, string?][] = [
    ["endpoint", "Endpoint (optional, empty for AWS)", "url"],
    ["bucket", "Bucket"],
    ["prefix", "Prefix (optional)"],
    ["region", "Region (optional)"],
    ["access_key", "Access key"],
    ["secret_key", "Secret key", "password"],
  ];
  const s = useSubmit(async () => {
    const fields = f.kind === "s3" ? s3.map(([k]) => k) : ["path"];
    const config: Record<string, string> = Object.fromEntries(fields.filter((k) => f[k]?.trim()).map((k) => [k, f[k].trim()]));
    if (f.kind === "s3" && f.storage_class !== "STANDARD") config.storage_class = f.storage_class;
    await post("/backups/targets", { name: f.name.trim(), kind: f.kind, config });
    props.onSaved();
    props.onClose();
  });
  const input = (k: string, label: string, type?: string) => <ConfigInput key={k} label={label} type={type} value={f[k]} onChange={(v) => set({ [k]: v })} />;
  return (
    <Modal open onClose={props.onClose} title="Add backup target">
      <form class="space-y-4" onSubmit={s.submit}>
        <div class="grid gap-4 sm:grid-cols-2">
          {input("name", "Name")}
          <Field label="Kind">
            <Select
              value={f.kind}
              onChange={(kind) => set({ kind })}
              options={[
                ["s3", "S3-compatible storage"],
                ["local", "Folder on this server"],
              ]}
            />
          </Field>
        </div>
        {f.kind === "s3" ? s3.map(([k, label, type]) => input(k, label, type)) : input("path", "Folder")}
        {f.kind === "s3" && (
          <Field label="Storage class" hint={storageHints[f.storage_class]}>
            <Select value={f.storage_class} onChange={(storage_class) => set({ storage_class })} options={storageClasses} />
          </Field>
        )}
        <FormButtons s={s} submit="Add target" onCancel={props.onClose} />
      </form>
    </Modal>
  );
}

type ServiceRef = { id: string; name: string; project: string; kind: string };
/** Every service the user can see, or null while loading. */
function useServices() {
  const r = useApi<{ services: ServiceRef[] }>("/services");
  return r.data?.services ?? (r.error ? [] : null);
}

function JobForm(props: { targets: Target[]; services: ServiceRef[] | null; onClose: () => void; onSaved: () => void }) {
  const { f, set, bind } = useForm({ kind: "database", service: "", volume: "", target: props.targets[0]?.id ?? "", schedule: "0 3 * * *", keep: "7" });
  const detail = useApi<{ service: { spec: { mounts?: { type: string; name?: string; source?: string }[] } } }>(f.kind === "volume" && f.service ? `/services/${f.service}` : null);
  // The service's named volumes and the folders of its server it mounts (bind mounts).
  const volumes = (detail.data?.service.spec.mounts ?? []).flatMap((m): [string, string][] =>
    m.type === "volume" && m.name ? [[m.name, m.name]] : m.type === "bind" && m.source ? [[m.source, `Host folder ${m.source}`]] : [],
  );
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
            onChange={(kind) => set({ kind, service: "" })}
            options={[
              ["database", "A database (consistent dump)"],
              ["volume", "A volume or host folder"],
              ["state", "Tugport's own state"],
            ]}
          />
        </Field>
        {f.kind !== "state" && (
          <Field label="Service">
            <Select
              value={f.service}
              onChange={(service) => set({ service, volume: "" })}
              options={[
                ["", props.services ? "Choose…" : "Loading…"],
                ...(props.services ?? []).filter((x) => f.kind !== "database" || x.kind === "database").map((x): [string, string] => [x.id, `${x.project}.${x.name}`]),
              ]}
            />
          </Field>
        )}
        {f.kind === "volume" && f.service && (
          <Field
            label="Volume or folder"
            hint={
              detail.data && !volumes.length
                ? "This service has no named volumes or host folders to back up."
                : f.volume.startsWith("/")
                  ? "A folder of the server the service runs on, which the service mounts."
                  : undefined
            }
          >
            <Select class="font-mono" value={f.volume} onChange={(volume) => set({ volume })} options={[["", detail.data ? "Choose…" : "Loading…"], ...volumes]} />
          </Field>
        )}
        <div class="grid gap-4 sm:grid-cols-3">
          <Field label="Target">
            <Select value={f.target} onChange={(target) => set({ target })} options={props.targets.map((t): [string, string] => [t.id, t.name])} />
          </Field>
          <Input label="Schedule" hint="Use cron format, in UTC." class="font-mono" required {...bind("schedule")} />
          <Input label="Keep the last" type="number" min={1} {...bind("keep")} />
        </div>
        <FormButtons s={s} submit="Add job" onCancel={props.onClose} disabled={(f.kind !== "state" && !f.service) || (f.kind === "volume" && !f.volume)} />
      </form>
    </Modal>
  );
}

function Runs(props: { job: Job; archived: boolean; onLog: (id: string) => void; onClose: () => void }) {
  const r = useApi<{ runs: Run[] }>(`/backups/jobs/${props.job.id}/runs`, (e) => e.kind === "backup");
  const restore = async (id: string, test: boolean, full = false) => {
    const res = await post<{ message?: string }>(`/backups/runs/${id}/restore`, { test, full });
    toast(res.message ? sentence(res.message) : test ? "The test restore finished." : "The backup was restored.");
    await r.reload();
  };
  return (
    <Modal open onClose={props.onClose} title="Backup runs" wide>
      {props.archived && (
        <p class="mb-3 text-sm text-muted">
          These backups are in Glacier Deep Archive. Test restore is quick and free: it lists the snapshot and checks the repository's index and folder trees, which stay readable, but
          not the file data, which stays frozen. Full test has S3 thaw the data first (up to 48 hours, and a bulk retrieval fee of about $0.0025 per GB), then restores it into a
          throwaway copy and checks it, without touching the service.
        </p>
      )}
      <Loaded r={r}>
        {(d) =>
          d.runs.length === 0 ? (
            <Empty>This job has not run yet.</Empty>
          ) : (
            <ul class="inset-list">
              {d.runs.map((run) => (
                <li key={run.id} class="flex flex-wrap items-center gap-3 px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <Pill state={run.status} />
                      <span class="text-dim">{when(run.started_at)}</span>
                      {run.size_bytes != null && <span class="text-muted">{bytes(run.size_bytes)}</span>}
                    </div>
                    {run.status === "running" && <Progress id={run.id} initial={run.progress} class="mt-1" />}
                    {run.error && <p class="text-bad">{run.error}</p>}
                  </div>
                  <div class="flex flex-wrap gap-2">
                    <button class="btn" onClick={() => props.onLog(run.id)}>
                      Log
                    </button>
                    {run.snapshot && (
                      <>
                        <Action run={() => restore(run.id, true)}>Test restore</Action>
                        {props.archived && (
                          <Action
                            confirm="Run a full test restore? S3 first thaws this backup's data from Glacier Deep Archive, which takes up to 48 hours and costs a bulk retrieval fee of about $0.0025 per GB. It is then restored into a throwaway copy and checked; the service is not touched. The run reports the result."
                            run={() => restore(run.id, true, true)}
                          >
                            Full test
                          </Action>
                        )}
                        <Action
                          class="btn btn-danger"
                          confirm={
                            props.archived
                              ? "Restore this backup? It replaces the current data. It is in Glacier Deep Archive, so S3 first thaws it, which can take up to 48 hours."
                              : "Restore this backup? It replaces the current data."
                          }
                          run={() => restore(run.id, false)}
                        >
                          Restore
                        </Action>
                      </>
                    )}
                  </div>
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
  const r = useApi<{ enabled: boolean; account?: string; tunnel_id?: string; running?: boolean; domains?: string[] }>("/tunnel");
  const [token, setToken] = useState("");
  const s = useSubmit(async () => {
    await put("/tunnel", { api_token: token.trim() });
    setToken("");
    await r.reload();
  });
  return (
    <Card title="Cloudflare Tunnel">
      <Loaded r={r}>
        {(t) =>
          t.enabled ? (
            <div class="space-y-3">
              <p class="flex flex-wrap items-center gap-2">
                <Pill state={t.running ? "running" : "stopped"} /> <span class="text-dim">Tunnel {t.tunnel_id}</span>
                {t.account && <span class="text-muted">on account {t.account}</span>}
              </p>
              <p class="text-dim">Domains with the tunnel switch on reach this server through Cloudflare, so the server needs no open ports for them.</p>
              <p class="text-dim">
                {t.domains?.length ? `Routed through it: ${t.domains.join(", ")}.` : "No domain uses the tunnel yet; turn on the tunnel switch in a domain's settings."}
              </p>
              <Action
                class="btn btn-danger"
                confirm="Disconnect the tunnel? Domains routed through it stop working until you connect again."
                run={() => del("/tunnel").then(r.reload)}
              >
                Disconnect
              </Action>
            </div>
          ) : (
            <form class="space-y-4" onSubmit={s.submit}>
              <p class="text-dim">With an API token, Tugport creates a tunnel, runs cloudflared and adds the DNS records for every domain that uses the tunnel.</p>
              <Input
                label="Cloudflare API token"
                hint="It needs the Cloudflare Tunnel edit and DNS edit permissions for your zones."
                class="font-mono"
                type="password"
                required
                autoComplete="off"
                value={token}
                onInput={(e) => setToken(e.currentTarget.value)}
              />
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
  const me = useMe();
  return (
    <>
      <AboutCard />
      {me.principal.is_owner && <Updates />}
    </>
  );
}

function Updates() {
  const r = useApi<{ current: string; latest: { version: string; notes: string } | null; available: boolean }>("/updates");
  return (
    <Card title="Updates" actions={<Action run={r.reload}>Check again</Action>}>
      <Loaded r={r}>
        {(u) =>
          !u.latest ? (
            <p class="text-dim">Tugport could not look up the newest release on GitHub just now, so check again later.</p>
          ) : u.available ? (
            <div class="space-y-3">
              <p>
                Tugport {u.latest.version} is available; this server runs {u.current}.
              </p>
              {u.latest.notes && (
                <details>
                  <summary class="text-sm text-dim">Release notes</summary>
                  <pre class="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded-md border border-line bg-bg p-3 text-xs">{u.latest.notes}</pre>
                </details>
              )}
              <Action
                class="btn btn-primary"
                confirm={`Install Tugport ${u.latest.version}? Running deploys finish first, and traffic keeps flowing during the switch.`}
                run={async () => toast(`Updating to ${(await post<{ updating_to: string }>("/updates/apply")).updating_to}. The panel reconnects by itself.`)}
              >
                Install {u.latest.version}
              </Action>
            </div>
          ) : (
            <p class="text-dim">Tugport {u.current} is the newest release.</p>
          )
        }
      </Loaded>
    </Card>
  );
}

function AboutCard() {
  const r = useApi<{ version: string; commit: string; source: string; docker: string | null }>("/about");
  return (
    <Card title="About Tugport">
      <Loaded r={r}>
        {(a) => (
          <dl class="grid grid-cols-[8rem_1fr] gap-y-2">
            <Row label="Version">{a.version}</Row>
            <Row label="Commit">
              <code class="break-all">{a.commit}</code>
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
