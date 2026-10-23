import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { atLeast, patch, useApi, type Node, type Spec } from "../api";
import { Card, Check, ErrorBox, Field, Note, Select, toast, useSubmit } from "../ui";
import { joinArgs, labelsText, lines, matchServers, mergePatch, num, parseLabels, pinReasons, splitArgs } from "../util";
import { BUILDS, PROVIDER_KINDS } from "./new-service";
import { RedeployNote, type Ctx } from "./service";

type Port = { container: string; host: string; protocol: string };
type MountRow = { type: string; name: string; source: string; target: string; readonly: boolean };
const str = (v: unknown) => (v == null ? "" : String(v));

function toForm(spec: Spec) {
  const src = spec.source;
  const b = spec.build ?? { type: "auto" };
  return {
    srcType: src.type as string,
    provider: src.type === "git" ? (src.provider ?? "") : "",
    repo: src.type === "git" ? src.repo : "",
    branch: src.type === "git" ? src.branch : "main",
    path: src.type === "git" ? src.path : "",
    watch: src.type === "git" ? src.watch_paths.join("\n") : "",
    auto: src.type === "git" ? src.auto_deploy : true,
    image: src.type === "image" ? src.image : "",
    content: src.type === "dockerfile" ? src.content : "",
    yaml: src.type === "compose" ? src.yaml : "",
    composeFile: spec.compose?.file ?? "docker-compose.yml",
    build: b.type as string,
    dockerfile: b.path ?? "Dockerfile",
    target: b.target ?? "",
    builder: b.builder ?? "heroku/builder:24",
    buildArgs: (spec.build_args ?? []).join("\n"),
    command: spec.command ? joinArgs(spec.command) : "",
    entrypoint: spec.entrypoint ? joinArgs(spec.entrypoint) : "",
    workdir: spec.working_dir ?? "",
    user: spec.user ?? "",
    replicas: str(spec.replicas),
    node: spec.placement?.node ?? "",
    region: spec.placement?.region ?? "",
    labels: labelsText(spec.placement?.labels),
    cpus: str(spec.resources?.cpus),
    memory: str(spec.resources?.memory_mb),
    reservation: str(spec.resources?.memory_reservation_mb),
    hKind: spec.health?.kind ?? "auto",
    hPath: spec.health?.path ?? "/",
    hPort: str(spec.health?.port),
    hCommand: spec.health?.command ?? "",
    hInterval: str(spec.health?.interval_s),
    hTimeout: str(spec.health?.timeout_s),
    hRetries: str(spec.health?.retries),
    hStart: str(spec.health?.start_period_s),
    strategy: spec.rollout?.strategy ?? "start_first",
    drain: str(spec.rollout?.drain_s),
    healthTimeout: str(spec.rollout?.health_timeout_s),
    keepImages: str(spec.rollout?.keep_images),
    policy: spec.deploy_policy ?? "",
    ports: ((spec.ports ?? []) as { container: number; host: number; protocol: string }[]).map(
      (p): Port => ({ container: str(p.container), host: str(p.host), protocol: p.protocol }),
    ),
    mounts: ((spec.mounts ?? []) as Record<string, any>[])
      .filter((m) => m.type !== "file")
      .map((m): MountRow => ({ type: m.type, name: m.name ?? "", source: m.source ?? "", target: m.target, readonly: !!m.readonly })),
    gpu: !!spec.gpu,
    nnp: !!spec.no_new_privileges,
    privileged: !!spec.privileged,
    socket: !!spec.docker_socket,
    hostNet: !!spec.host_network,
    capAdd: (spec.cap_add ?? []).join("\n"),
    capDrop: (spec.cap_drop ?? []).join("\n"),
    auEnabled: !!spec.auto_update?.enabled,
    auInterval: str(spec.auto_update?.interval_minutes ?? 360),
    auPolicy: spec.auto_update?.policy ?? "digest",
  };
}
type Form = ReturnType<typeof toForm>;

function toSpec(spec: Spec, f: Form, kind: string): Spec {
  const next: Spec = structuredClone(spec);
  if (f.srcType === "git") {
    next.source = { type: "git", provider: f.provider || null, repo: f.repo.trim(), branch: f.branch.trim() || "main", path: f.path.trim(), watch_paths: lines(f.watch), auto_deploy: f.auto };
  } else if (f.srcType === "image") next.source = { type: "image", image: f.image.trim() };
  else if (f.srcType === "dockerfile") next.source = { type: "dockerfile", content: f.content };
  else next.source = { type: "compose", yaml: f.yaml };
  if (kind === "compose" && f.srcType === "git") next.compose = { ...(spec.compose ?? {}), file: f.composeFile.trim() || "docker-compose.yml" };
  next.build =
    f.build === "dockerfile"
      ? { type: "dockerfile", path: f.dockerfile.trim() || "Dockerfile", target: f.target.trim() || null }
      : f.build === "buildpacks"
        ? { type: "buildpacks", builder: f.builder.trim() || "heroku/builder:24" }
        : { type: f.build };
  next.build_args = lines(f.buildArgs);
  next.command = f.command.trim() ? splitArgs(f.command) : null;
  next.entrypoint = f.entrypoint.trim() ? splitArgs(f.entrypoint) : null;
  next.working_dir = f.workdir.trim() || null;
  next.user = f.user.trim() || null;
  next.replicas = num(f.replicas) ?? 1;
  // Labels that do not parse keep their saved value; the form says what is wrong with them.
  next.placement = { node: f.node.trim() || null, region: f.region.trim() || null, labels: parseLabels(f.labels) ?? spec.placement?.labels ?? {} };
  next.resources = { cpus: num(f.cpus), memory_mb: num(f.memory), memory_reservation_mb: num(f.reservation) };
  next.health = {
    kind: f.hKind,
    path: f.hPath.trim() || "/",
    port: num(f.hPort),
    command: f.hCommand.trim() || null,
    interval_s: num(f.hInterval) ?? 5,
    timeout_s: num(f.hTimeout) ?? 5,
    retries: num(f.hRetries) ?? 3,
    start_period_s: num(f.hStart) ?? 0,
  };
  next.rollout = { strategy: f.strategy, drain_s: num(f.drain) ?? 10, health_timeout_s: num(f.healthTimeout) ?? 180, keep_images: num(f.keepImages) ?? 3 };
  next.deploy_policy = f.policy || null;
  next.ports = f.ports.filter((p) => p.container).map((p) => ({ container: Number(p.container), host: Number(p.host || p.container), protocol: p.protocol }));
  next.mounts = [
    ...f.mounts
      .filter((m) => m.target.trim())
      .map((m) =>
        m.type === "bind"
          ? { type: "bind", source: m.source.trim(), target: m.target.trim(), readonly: m.readonly }
          : { type: "volume", name: m.name.trim(), target: m.target.trim(), readonly: m.readonly },
      ),
    ...(spec.mounts ?? []).filter((m: { type: string }) => m.type === "file"),
  ];
  next.gpu = f.gpu;
  next.no_new_privileges = f.nnp;
  next.privileged = f.privileged;
  next.docker_socket = f.socket;
  next.host_network = f.hostNet;
  next.cap_add = lines(f.capAdd);
  next.cap_drop = lines(f.capDrop);
  next.auto_update = { enabled: f.auEnabled, interval_minutes: num(f.auInterval) ?? 360, policy: f.auPolicy };
  return next;
}

export function ServiceSettings({ c }: { c: Ctx }) {
  const [saved, setSaved] = useState(false);
  const [json, setJson] = useState(false);
  const s = c.d.service;
  return (
    <div class="max-w-3xl space-y-6">
      {saved && (
        <Note tone="ok">
          <RedeployNote c={c}>Saved. The new settings take effect with the next deploy.</RedeployNote>
        </Note>
      )}
      <div class="flex justify-end">
        <button class="btn" onClick={() => setJson(!json)}>
          {json ? "Back to the form" : "Edit as JSON"}
        </button>
      </div>
      {json ? (
        <JsonEditor key={s.updated_at} c={c} onSaved={() => setSaved(true)} />
      ) : (
        <SettingsForm key={s.updated_at} c={c} onSaved={() => setSaved(true)} />
      )}
    </div>
  );
}

function SettingsForm({ c, onSaved }: { c: Ctx; onSaved: () => void }) {
  const s = c.d.service;
  const canEdit = atLeast(c.d.role, "operator");
  const [f, setF] = useState(() => toForm(s.spec));
  const set = (p: Partial<Form>) => setF((x) => ({ ...x, ...p }));
  const text = (k: keyof Form) => ({ value: f[k] as string, onInput: (e: { currentTarget: { value: string } }) => set({ [k]: e.currentTarget.value }) });
  const providers = useApi<{ providers: { id: string; kind: string; name: string }[] }>(f.srcType === "git" ? "/git/providers" : null);
  const next = toSpec(s.spec, f, s.kind);
  const changes = mergePatch(s.spec, next);
  const dirty = Object.keys(changes).length > 0;
  const labelsOk = parseLabels(f.labels) !== null;
  const sub = useSubmit(async () => {
    if (!labelsOk) throw new Error("Write each placement label as key=value, separated by commas or new lines.");
    await patch(`/services/${s.id}`, { spec: changes });
    toast("Settings saved.");
    onSaved();
    await c.reload();
  });
  const isDb = s.kind === "database";
  const isCompose = s.kind === "compose";
  const builds = f.srcType === "git" && !isCompose;

  return (
    <form onSubmit={sub.submit}>
      <fieldset disabled={!canEdit} class="space-y-6">
        {!canEdit && <Note>Changing settings needs the operator role on this project.</Note>}

        <Section title="Source">
          {isDb ? (
            <Field label="Image" hint="The image is pinned to an exact version, and major upgrades are manual, so take a backup before changing the major version.">
              <input class="input font-mono" spellcheck={false} {...text("image")} />
            </Field>
          ) : (
            <>
              <Field label="Type">
                <Select
                  value={f.srcType}
                  onChange={(srcType) => set({ srcType })}
                  options={
                    isCompose
                      ? [
                          ["compose", "Pasted Compose file"],
                          ["git", "Compose file from a repository"],
                        ]
                      : [
                          ["git", "Git repository"],
                          ["image", "Docker image"],
                          ["dockerfile", "Inline Dockerfile"],
                        ]
                  }
                />
              </Field>
              {f.srcType === "git" && (
                <>
                  <Field label="Repository from">
                    <Select
                      value={f.provider}
                      onChange={(provider) => set({ provider })}
                      options={[
                        ["", "A URL (public, or private with a deploy key)"],
                        ...(providers.data?.providers ?? []).map((p): [string, string] => [p.id, `${p.name} (${PROVIDER_KINDS[p.kind] ?? p.kind})`]),
                      ]}
                    />
                  </Field>
                  <Field label={f.provider ? "Repository" : "Repository URL"}>
                    <input class="input font-mono" required spellcheck={false} placeholder={f.provider ? "owner/name" : "https://github.com/owner/app.git"} {...text("repo")} />
                  </Field>
                  <div class="grid gap-4 sm:grid-cols-2">
                    <Field label="Branch">
                      <input class="input font-mono" spellcheck={false} {...text("branch")} />
                    </Field>
                    {isCompose ? (
                      <Field label="Compose file">
                        <input class="input font-mono" spellcheck={false} {...text("composeFile")} />
                      </Field>
                    ) : (
                      <Field label="Path" hint="This is the build context inside the repository.">
                        <input class="input font-mono" spellcheck={false} {...text("path")} />
                      </Field>
                    )}
                  </div>
                  {!isCompose && (
                    <Field label="Watch paths" hint="Redeploy only when a push changes these paths, one glob per line. Leave this empty to redeploy on any change.">
                      <textarea class="input h-20" spellcheck={false} {...text("watch")} />
                    </Field>
                  )}
                  <Check checked={f.auto} onChange={(auto) => set({ auto })} label="Deploy on every push" />
                </>
              )}
              {f.srcType === "image" && (
                <Field label="Image">
                  <input class="input font-mono" required spellcheck={false} {...text("image")} />
                </Field>
              )}
              {f.srcType === "dockerfile" && (
                <Field label="Dockerfile">
                  <textarea class="input h-56" spellcheck={false} {...text("content")} />
                </Field>
              )}
              {f.srcType === "compose" && (
                <Field label="docker-compose.yml">
                  <textarea class="input h-72" spellcheck={false} {...text("yaml")} />
                </Field>
              )}
            </>
          )}
        </Section>

        {builds && (
          <Section title="Build">
            <Field label="Build method">
              <Select value={f.build} onChange={(build) => set({ build })} options={BUILDS} />
            </Field>
            {f.build === "dockerfile" && (
              <div class="grid gap-4 sm:grid-cols-2">
                <Field label="Dockerfile path">
                  <input class="input font-mono" spellcheck={false} {...text("dockerfile")} />
                </Field>
                <Field label="Target stage">
                  <input class="input font-mono" spellcheck={false} {...text("target")} />
                </Field>
              </div>
            )}
            {f.build === "buildpacks" && (
              <Field label="Builder image">
                <input class="input font-mono" spellcheck={false} {...text("builder")} />
              </Field>
            )}
            <Field label="Variables passed as build arguments" hint="List environment variable names, one per line, to pass in addition to the ARGs the Dockerfile declares.">
              <textarea class="input h-16" spellcheck={false} {...text("buildArgs")} />
            </Field>
          </Section>
        )}

        <Section title="Runtime">
          <div class="grid gap-4 sm:grid-cols-2">
            <Field label="Command" hint="This replaces the image's command; quote arguments that contain spaces.">
              <input class="input font-mono" spellcheck={false} placeholder="npm run start" {...text("command")} />
            </Field>
            <Field label="Entrypoint">
              <input class="input font-mono" spellcheck={false} {...text("entrypoint")} />
            </Field>
            <Field label="Working directory">
              <input class="input font-mono" spellcheck={false} {...text("workdir")} />
            </Field>
            <Field label="User">
              <input class="input font-mono" spellcheck={false} placeholder="1000:1000" {...text("user")} />
            </Field>
          </div>
        </Section>

        {isCompose ? (
          <Section title="Placement">
            <p class="text-dim">Compose stacks run on the panel's server, so placement rules and the replica count do not apply to them.</p>
          </Section>
        ) : (
          <Placement c={c} f={f} set={set} next={next} moved={"placement" in changes} />
        )}

        <Section title="Resources">
          <div class="grid gap-4 sm:grid-cols-3">
            <Field label="CPU limit (cores)">
              <input class="input" type="number" min={0} step="0.1" placeholder="No limit" {...text("cpus")} />
            </Field>
            <Field label="Memory limit (MB)">
              <input class="input" type="number" min={0} placeholder="No limit" {...text("memory")} />
            </Field>
            <Field label="Memory reservation (MB)">
              <input class="input" type="number" min={0} placeholder="None" {...text("reservation")} />
            </Field>
          </div>
        </Section>

        <Section title="Health check">
          <Field label="Check" hint={HEALTH_HELP[f.hKind]}>
            <Select
              value={f.hKind}
              onChange={(hKind) => set({ hKind })}
              options={[
                ["auto", "Automatic"],
                ["http", "HTTP request"],
                ["tcp", "TCP connection"],
                ["command", "Command inside the container"],
                ["none", "None"],
              ]}
            />
          </Field>
          <div class="grid gap-4 sm:grid-cols-2">
            {f.hKind === "http" && (
              <Field label="Path">
                <input class="input font-mono" {...text("hPath")} />
              </Field>
            )}
            {(f.hKind === "http" || f.hKind === "tcp") && (
              <Field label="Port" hint="Leave this empty to use the first domain's port.">
                <input class="input" type="number" min={1} max={65535} {...text("hPort")} />
              </Field>
            )}
            {f.hKind === "command" && (
              <Field label="Command" class="sm:col-span-2">
                <input class="input font-mono" spellcheck={false} placeholder="pg_isready -U app" {...text("hCommand")} />
              </Field>
            )}
          </div>
          {f.hKind !== "none" && (
            <div class="grid grid-cols-2 gap-4 sm:grid-cols-4">
              <Field label="Interval (s)">
                <input class="input" type="number" min={1} {...text("hInterval")} />
              </Field>
              <Field label="Timeout (s)">
                <input class="input" type="number" min={1} {...text("hTimeout")} />
              </Field>
              <Field label="Retries">
                <input class="input" type="number" min={0} {...text("hRetries")} />
              </Field>
              <Field label="Start period (s)">
                <input class="input" type="number" min={0} {...text("hStart")} />
              </Field>
            </div>
          )}
        </Section>

        <Section title="Rollout">
          <Field
            label="Strategy"
            hint={
              f.strategy === "stop_first"
                ? "The old version stops first, which suits workers that must never run twice, like bots."
                : "The new version starts, gets traffic once it is healthy, and then the old one stops."
            }
          >
            <Select
              value={f.strategy}
              onChange={(strategy) => set({ strategy })}
              disabled={isDb}
              options={[
                ["start_first", "Start the new version first (zero downtime)"],
                ["stop_first", "Stop the old version first"],
              ]}
            />
          </Field>
          <div class="grid grid-cols-2 gap-4 sm:grid-cols-3">
            <Field label="Drain time (s)" hint="Old replicas get this long to finish their requests.">
              <input class="input" type="number" min={0} {...text("drain")} />
            </Field>
            <Field label="Health timeout (s)" hint="A new version gets this long to become healthy.">
              <input class="input" type="number" min={1} {...text("healthTimeout")} />
            </Field>
            <Field label="Images kept" hint="Old images stay for one-click rollbacks.">
              <input class="input" type="number" min={1} {...text("keepImages")} />
            </Field>
          </div>
          <Field label="When a deploy starts while another one runs">
            <Select
              value={f.policy}
              onChange={(policy) => set({ policy })}
              options={[
                ["", "Use the instance default"],
                ["cancel", "Cancel the running deploy"],
                ["queue", "Wait for the running deploy"],
              ]}
            />
          </Field>
        </Section>

        <Section title="Published ports">
          <p class="text-xs text-muted">Published ports open on the server's own address, which suits TCP and UDP services. HTTP apps should get a domain instead.</p>
          {f.ports.map((p, i) => (
            <div key={i} class="grid grid-cols-[1fr_1fr_6rem_auto] items-end gap-2">
              <Field label="Container port">
                <input class="input" type="number" min={1} max={65535} value={p.container} onInput={(e) => set({ ports: f.ports.map((x, j) => (j === i ? { ...x, container: e.currentTarget.value } : x)) })} />
              </Field>
              <Field label="Server port">
                <input class="input" type="number" min={1} max={65535} value={p.host} placeholder={p.container} onInput={(e) => set({ ports: f.ports.map((x, j) => (j === i ? { ...x, host: e.currentTarget.value } : x)) })} />
              </Field>
              <Field label="Protocol">
                <Select value={p.protocol} options={["tcp", "udp"]} onChange={(protocol) => set({ ports: f.ports.map((x, j) => (j === i ? { ...x, protocol } : x)) })} />
              </Field>
              <button type="button" class="btn btn-ghost text-bad" onClick={() => set({ ports: f.ports.filter((_, j) => j !== i) })}>
                Remove
              </button>
            </div>
          ))}
          <button type="button" class="btn" onClick={() => set({ ports: [...f.ports, { container: "", host: "", protocol: "tcp" }] })}>
            Add port
          </button>
        </Section>

        <Section title="Mounts">
          <p class="text-xs text-muted">
            Volumes keep data across deploys. Bind mounts reach into the server's file system, so they need the admin role and the destroy grant. Files are managed on the Files tab.
          </p>
          {f.mounts.map((m, i) => {
            const upd = (p: Partial<MountRow>) => set({ mounts: f.mounts.map((x, j) => (j === i ? { ...x, ...p } : x)) });
            return (
              <div key={i} class="grid items-end gap-2 sm:grid-cols-[7rem_1fr_1fr_auto_auto]">
                <Field label="Type">
                  <Select value={m.type} options={[["volume", "Volume"], ["bind", "Bind"]]} onChange={(type) => upd({ type })} />
                </Field>
                {m.type === "bind" ? (
                  <Field label="Server path">
                    <input class="input font-mono" placeholder="/srv/data" value={m.source} onInput={(e) => upd({ source: e.currentTarget.value })} />
                  </Field>
                ) : (
                  <Field label="Volume name">
                    <input class="input font-mono" value={m.name} onInput={(e) => upd({ name: e.currentTarget.value })} />
                  </Field>
                )}
                <Field label="Path in the container">
                  <input class="input font-mono" value={m.target} onInput={(e) => upd({ target: e.currentTarget.value })} />
                </Field>
                <label class="flex items-center gap-2 pb-2">
                  <input type="checkbox" checked={m.readonly} onChange={(e) => upd({ readonly: e.currentTarget.checked })} />
                  Read-only
                </label>
                <button type="button" class="btn btn-ghost text-bad" onClick={() => set({ mounts: f.mounts.filter((_, j) => j !== i) })}>
                  Remove
                </button>
              </div>
            );
          })}
          {isDb && f.mounts.length > 0 && <p class="text-xs text-warn">Removing the data volume of a database loses its files on the next deploy.</p>}
          <button type="button" class="btn" onClick={() => set({ mounts: [...f.mounts, { type: "volume", name: `${s.name}-data`, source: "", target: "", readonly: false }] })}>
            Add mount
          </button>
        </Section>

        <Section title="Container security">
          <Check
            checked={f.nnp}
            onChange={(nnp) => set({ nnp })}
            label="No new privileges"
            hint="This stops setuid programs from gaining privileges inside the container, so leave it on unless the app needs sudo or something similar."
          />
          <Check checked={f.gpu} onChange={(gpu) => set({ gpu })} label="GPU access" hint="The containers get access to the server's GPUs." />
          <div class="grid gap-4 sm:grid-cols-2">
            <Field label="Add capabilities" hint="Write one per line, like NET_ADMIN; adding them needs the destroy grant.">
              <textarea class="input h-16" spellcheck={false} {...text("capAdd")} />
            </Field>
            <Field label="Drop capabilities" hint="Write one per line, like ALL.">
              <textarea class="input h-16" spellcheck={false} {...text("capDrop")} />
            </Field>
          </div>
          <div class="space-y-3 rounded-md border border-bad/40 p-3">
            <p class="text-xs text-bad">These switches give the service control over the whole server. Turning them on needs the admin role and the destroy grant.</p>
            <Check checked={f.privileged} onChange={(privileged) => set({ privileged })} label="Privileged mode" />
            <Check checked={f.socket} onChange={(socket) => set({ socket })} label="Docker socket" hint="The service can then start and stop any container on the server." />
            <Check checked={f.hostNet} onChange={(hostNet) => set({ hostNet })} label="Host networking" hint="The service then shares the server's network stack instead of the project network." />
          </div>
        </Section>

        {f.srcType === "image" && !isDb && (
          <Section title="Image auto-update">
            <Check checked={f.auEnabled} onChange={(auEnabled) => set({ auEnabled })} label="Update the image automatically" hint="New images roll out with health checks and roll back if they fail." />
            {f.auEnabled && (
              <div class="grid gap-4 sm:grid-cols-2">
                <Field label="Check every (minutes)">
                  <input class="input" type="number" min={5} {...text("auInterval")} />
                </Field>
                <Field label="Follow">
                  <Select
                    value={f.auPolicy}
                    onChange={(auPolicy) => set({ auPolicy })}
                    options={[
                      ["digest", "New images for the same tag"],
                      ["same_major", "Newer versions with the same major number"],
                    ]}
                  />
                </Field>
              </div>
            )}
          </Section>
        )}
      </fieldset>

      {canEdit && dirty && (
        <div class="sticky bottom-0 z-10 -mx-4 mt-6 flex flex-wrap items-center justify-between gap-2 border-t border-line bg-surface px-4 py-3 md:mx-0 md:rounded-lg md:border">
          <span class="text-dim">You have unsaved changes.</span>
          <span class="flex gap-2">
            <button type="button" class="btn" onClick={() => setF(toForm(s.spec))}>
              Discard
            </button>
            <button class="btn btn-primary" disabled={sub.busy}>
              Save settings
            </button>
          </span>
          {sub.error && (
            <div class="w-full">
              <ErrorBox error={sub.error} />
            </div>
          )}
        </div>
      )}
    </form>
  );
}

const HEALTH_HELP: Record<string, string> = {
  auto: "Tugport uses the image's HEALTHCHECK if it has one, then an HTTP check on the first domain's port, and otherwise only checks that the container keeps running.",
  http: "A new version gets traffic once this path answers with a 2xx or 3xx status.",
  tcp: "A new version gets traffic once the port accepts connections.",
  command: "A new version gets traffic once this command exits with status 0.",
  none: "Traffic moves to a new version as soon as its container starts.",
};

function Section(props: { title: string; children: ComponentChildren }) {
  return (
    <Card title={props.title}>
      <div class="space-y-4">{props.children}</div>
    </Card>
  );
}

const and = (l: string[]) => new Intl.ListFormat("en", { type: "conjunction" }).format(l);

type Rules = { node: string; region: string; labels: string };

/** Server, region and label rules, and the servers they match right now. `children` come first in the grid. */
export function PlacementFields(props: { nodes: Node[] | undefined; rules: Rules; set: (p: Partial<Rules>) => void; children?: ComponentChildren }) {
  const { nodes, rules: f, set } = props;
  const labels = parseLabels(f.labels);
  const matches = nodes && labels ? matchServers(nodes, { node: f.node.trim() || null, region: f.region.trim() || null, labels }) : null;
  // A placement may name a server by id or by name; keep whichever the spec uses.
  const options: [string, string][] = (nodes ?? []).map((n) => [
    f.node === n.name ? n.name : n.id,
    n.local ? `${n.name} (runs the panel)` : n.status === "online" ? n.name : `${n.name} (${n.status})`,
  ]);
  if (f.node && !options.some(([v]) => v === f.node)) options.push([f.node, `${f.node} (not a server of this panel)`]);
  const regions = [...new Set((nodes ?? []).map((n) => n.region).filter((r): r is string => !!r))];
  return (
    <>
      <div class="grid gap-4 sm:grid-cols-2">
        {props.children}
        <Field label="Server" hint={nodes ? "Pin it to one server, or let Tugport choose among the matching ones." : "Write a server's name to pin it there, or leave this empty."}>
          {nodes ? (
            <Select value={f.node} onChange={(node) => set({ node })} options={[["", "Any server that matches"], ...options]} />
          ) : (
            <input class="input font-mono" spellcheck={false} placeholder="Any server" value={f.node} onInput={(e) => set({ node: e.currentTarget.value })} />
          )}
        </Field>
        <Field label="Region" hint="Leave this empty to allow every region.">
          <input class="input font-mono" spellcheck={false} list="tp-regions" placeholder="Any region" value={f.region} onInput={(e) => set({ region: e.currentTarget.value })} />
          <datalist id="tp-regions">
            {regions.map((r) => (
              <option key={r} value={r} />
            ))}
          </datalist>
        </Field>
        <Field label="Labels" hint="Write key=value pairs, separated by commas or new lines. Only servers with every one of these labels qualify.">
          <textarea class="input h-16" spellcheck={false} placeholder="disk=ssd" value={f.labels} onInput={(e) => set({ labels: e.currentTarget.value })} />
        </Field>
      </div>
      {!labels && <p class="text-sm text-bad">Write each label as key=value, separated by commas or new lines.</p>}
      {matches &&
        (matches.length ? (
          <p class="text-dim">Servers that match right now: {and(matches.map((n) => n.name))}.</p>
        ) : (
          <Note tone="bad">No online server matches these rules right now, so a deploy would fail.</Note>
        ))}
    </>
  );
}

/** The replica count and the rules that pick servers for the replicas. */
function Placement(props: { c: Ctx; f: Form; set: (p: Partial<Form>) => void; next: Spec; moved: boolean }) {
  const { f, set } = props;
  const kind = props.c.d.service.kind;
  const reasons = pinReasons(kind, props.next);
  return (
    <Section title="Placement and replicas">
      <PlacementFields nodes={props.c.nodes?.nodes} rules={f} set={set}>
        <Field
          label="Replicas"
          hint={
            kind === "database"
              ? "Databases run one primary, and read replicas have a tab of their own."
              : reasons.length
                ? "Every replica runs on the same server."
                : "Replicas spread over the matching servers, and each one gets traffic once it is healthy."
          }
        >
          <input class="input" type="number" min={0} max={100} disabled={kind === "database"} value={f.replicas} onInput={(e) => set({ replicas: e.currentTarget.value })} />
        </Field>
      </PlacementFields>
      {reasons.length > 0 && props.moved && (
        <Note tone="warn">
          Its data stays on the server it runs on now. If that server stops matching these rules, the next deploy starts the service on another server with empty volumes.
        </Note>
      )}
      <p class="text-xs text-muted">
        {reasons.length > 0 && `This service stays on one server because ${and(reasons)}. `}
        Databases and services with volumes, bind mounts, published ports, host networking or the Docker socket stay on one server, because their data or ports live there.
        Other services spread their replicas over every online server that matches.
      </p>
    </Section>
  );
}

/** The whole spec as JSON, for fields the form does not cover (sysctls, groups, aliases, networks). */
function JsonEditor({ c, onSaved }: { c: Ctx; onSaved: () => void }) {
  const s = c.d.service;
  const [text, setText] = useState(() => JSON.stringify(s.spec, null, 2));
  const sub = useSubmit(async () => {
    let next: Spec;
    try {
      next = JSON.parse(text);
    } catch (e) {
      throw new Error(`That is not valid JSON: ${(e as Error).message}`);
    }
    const changes = mergePatch(s.spec, next);
    if (!Object.keys(changes).length) throw new Error("Nothing changed.");
    await patch(`/services/${s.id}`, { spec: changes });
    toast("Settings saved.");
    onSaved();
    await c.reload();
  });
  return (
    <Card title="Service spec">
      <form class="space-y-3" onSubmit={sub.submit}>
        <p class="text-xs text-muted">Saving sends only the fields you changed, as a JSON merge patch. Removing a field resets it to its default.</p>
        <textarea class="input h-[60vh]" spellcheck={false} aria-label="Service spec as JSON" value={text} onInput={(e) => setText(e.currentTarget.value)} disabled={!atLeast(c.d.role, "operator")} />
        <ErrorBox error={sub.error} />
        {atLeast(c.d.role, "operator") && (
          <button class="btn btn-primary" disabled={sub.busy}>
            Save JSON
          </button>
        )}
      </form>
    </Card>
  );
}
