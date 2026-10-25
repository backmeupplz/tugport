import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { atLeast, patch, useApi, type Node, type Spec } from "../api";
import { Icon } from "../icons";
import { Card, Check, ErrorBox, Field, IconButton, Input, Note, Select, TextArea, UnsavedBar, toast, useForm, useSubmit } from "../ui";
import { LABELS_HINT, joinArgs, labelsText, lines, matchServers, mergePatch, num, parseLabels, patchAt, pinReasons, removeAt, splitArgs, str } from "../util";
import { BuildFields, buildSpec, gitSource, providerOptions } from "./new-service";
import { RedeployNote, type Ctx } from "./service";

type Port = { container: string; host: string; protocol: string };
type MountRow = { type: string; name: string; source: string; target: string; readonly: boolean };

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
    submodules: src.type === "git" ? (src.submodules ?? true) : true,
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
    ports: ((spec.ports ?? []) as { container: number; host: number; protocol: string }[]).map((p): Port => ({
      container: str(p.container),
      host: str(p.host),
      protocol: p.protocol,
    })),
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
  if (f.srcType === "git") next.source = gitSource(f);
  else if (f.srcType === "image") next.source = { type: "image", image: f.image.trim() };
  else if (f.srcType === "dockerfile") next.source = { type: "dockerfile", content: f.content };
  else next.source = { type: "compose", yaml: f.yaml };
  if (kind === "compose" && f.srcType === "git") next.compose = { ...(spec.compose ?? {}), file: f.composeFile.trim() || "docker-compose.yml" };
  next.build = buildSpec(f);
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
      {json ? <JsonEditor key={s.updated_at} c={c} onSaved={() => setSaved(true)} /> : <SettingsForm key={s.updated_at} c={c} onSaved={() => setSaved(true)} />}
      <button type="button" class="inline-flex items-center gap-1.5 text-sm text-muted hover:text-text" onClick={() => setJson(!json)}>
        <Icon name="code" />
        {json ? "Back to the form" : "Edit the raw settings as JSON"}
      </button>
    </div>
  );
}

function SettingsForm({ c, onSaved }: { c: Ctx; onSaved: () => void }) {
  const s = c.d.service;
  const canEdit = atLeast(c.d.role, "operator");
  const { f, set, bind, reset } = useForm(toForm(s.spec));
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
  const port = (i: number, p: Partial<Port>) => set({ ports: patchAt(f.ports, i, p) });
  const mount = (i: number, p: Partial<MountRow>) => set({ mounts: patchAt(f.mounts, i, p) });

  return (
    <form onSubmit={sub.submit}>
      <fieldset disabled={!canEdit} class="space-y-6">
        {!canEdit && <Note>Changing settings needs the operator role on this project.</Note>}

        <Section title="Source">
          {isDb ? (
            <Input
              label="Image"
              info="The image is pinned to an exact version, and major upgrades are manual, so take a backup before changing the major version."
              mono
              {...bind("image")}
            />
          ) : (
            <>
              <Field label="Type" info="Where the service comes from. A Git repository is built on the server; a Docker image is pulled as it is; an inline Dockerfile is built from the text you paste.">
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
                  <Field label="Repository from" info="A connected Git host (Settings, Git) gives Tugport access to private repositories and push webhooks. A plain URL works for public repositories, or for private ones with a deploy key.">
                    <Select value={f.provider} onChange={(provider) => set({ provider })} options={providerOptions(providers.data?.providers)} />
                  </Field>
                  <Input
                    label={f.provider ? "Repository" : "Repository URL"}
                    mono
                    required
                    placeholder={f.provider ? "owner/name" : "https://github.com/owner/app.git"}
                    {...bind("repo")}
                  />
                  <div class="grid gap-4 sm:grid-cols-2">
                    <Input label="Branch" info="Pushes to this branch deploy the service when deploying on every push is on." mono {...bind("branch")} />
                    {isCompose ? (
                      <Input label="Compose file" info="The Compose file's path inside the repository." mono {...bind("composeFile")} />
                    ) : (
                      <Input label="Path" info="This is the build context inside the repository." mono {...bind("path")} />
                    )}
                  </div>
                  {!isCompose && (
                    <TextArea
                      label="Watch paths"
                      info="Redeploy only when a push changes these paths, one glob per line. Leave this empty to redeploy on any change."
                      class="h-20"
                      {...bind("watch")}
                    />
                  )}
                  <Check checked={f.auto} onChange={(auto) => set({ auto })} label="Deploy on every push" info="A push to the branch builds and deploys the service. With this off, it deploys only from the panel, the CLI or its deploy webhook." />
                  <Check checked={f.submodules} onChange={(submodules) => set({ submodules })} label="Check out submodules" info="Deploys fetch the repository's Git submodules. Turn this off for services that don't use them, so their deploys skip that download." />
                </>
              )}
              {f.srcType === "image" && <Input label="Image" info="A Docker image reference like nginx:1.27 or ghcr.io/owner/app:latest. Private registries need their login under Settings, Registries." mono required {...bind("image")} />}
              {f.srcType === "dockerfile" && <TextArea label="Dockerfile" class="h-56" {...bind("content")} />}
              {f.srcType === "compose" && <TextArea label="docker-compose.yml" class="h-72" {...bind("yaml")} />}
            </>
          )}
        </Section>

        {builds && (
          <Section title="Build">
            <BuildFields f={f} set={set} bind={bind} />
            <TextArea
              label="Variables passed as build arguments"
              info="List environment variable names, one per line, to pass in addition to the ARGs the Dockerfile declares."
              class="h-16"
              {...bind("buildArgs")}
            />
          </Section>
        )}

        <Section title="Runtime">
          <div class="grid gap-4 sm:grid-cols-2">
            <Input label="Command" info="This replaces the image's command; quote arguments that contain spaces." mono placeholder="npm run start" {...bind("command")} />
            <Input label="Entrypoint" info="This replaces the image's entrypoint, the program the command is passed to. Leave it empty to keep the image's own." mono {...bind("entrypoint")} />
            <Input label="Working directory" info="The directory the command runs in. Leave it empty to keep the image's own." mono {...bind("workdir")} />
            <Input label="User" info="The user (and optionally group) the container runs as, by name or number. Leave it empty to keep the image's own." mono placeholder="1000:1000" {...bind("user")} />
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
            <Input label="CPU limit (cores)" info="The most CPU each replica may use, in cores, like 0.5 for half of one. It slows down above the limit instead of failing." type="number" min={0} step="0.1" placeholder="No limit" {...bind("cpus")} />
            <Input label="Memory limit (MB)" info="The most memory each replica may use. A replica that goes over is stopped and restarted." type="number" min={0} placeholder="No limit" {...bind("memory")} />
            <Input label="Memory reservation (MB)" info="Memory the server should keep free for each replica. When memory runs short, containers above their reservation are squeezed first." type="number" min={0} placeholder="None" {...bind("reservation")} />
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
            {f.hKind === "http" && <Input label="Path" info="The path the check requests, like /health." class="font-mono" {...bind("hPath")} />}
            {(f.hKind === "http" || f.hKind === "tcp") && (
              <Input label="Port" info="Leave this empty to use the first domain's port." type="number" min={1} max={65535} {...bind("hPort")} />
            )}
            {f.hKind === "command" && <Input label="Command" fieldClass="sm:col-span-2" mono placeholder="pg_isready -U app" {...bind("hCommand")} />}
          </div>
          {f.hKind !== "none" && (
            <div class="grid grid-cols-2 gap-4 sm:grid-cols-4">
              <Input label="Interval (s)" info="How often the check runs." type="number" min={1} {...bind("hInterval")} />
              <Input label="Timeout (s)" info="How long one check may take before it counts as failed." type="number" min={1} {...bind("hTimeout")} />
              <Input label="Retries" info="How many checks in a row must fail before a replica counts as unhealthy." type="number" min={0} {...bind("hRetries")} />
              <Input label="Start period (s)" info="Failed checks during this long after a start do not count, for apps that take a while to boot." type="number" min={0} {...bind("hStart")} />
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
            <Input label="Drain time (s)" info="Old replicas get this long to finish their requests." type="number" min={0} {...bind("drain")} />
            <Input label="Health timeout (s)" info="A new version gets this long to become healthy." type="number" min={1} {...bind("healthTimeout")} />
            <Input label="Images kept" info="Old images stay for one-click rollbacks." type="number" min={1} {...bind("keepImages")} />
          </div>
          <Field label="When a deploy starts while another one runs" info="Cancelling suits frequent pushes, since only the newest one matters. Waiting makes every deploy finish in order.">
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
              <Input label="Container port" info="The port the app listens on inside the container." type="number" min={1} max={65535} value={p.container} onInput={(e) => port(i, { container: e.currentTarget.value })} />
              <Input label="Server port" info="The port it opens on the server's own addresses; the same as the container port when left empty." type="number" min={1} max={65535} value={p.host} placeholder={p.container} onInput={(e) => port(i, { host: e.currentTarget.value })} />
              <Field label="Protocol">
                <Select value={p.protocol} options={["tcp", "udp"]} onChange={(protocol) => port(i, { protocol })} />
              </Field>
              <IconButton icon="trash" label="Remove this port" class="h-11 text-bad md:h-9" onClick={() => set({ ports: removeAt(f.ports, i) })} />
            </div>
          ))}
          <button type="button" class="btn" onClick={() => set({ ports: [...f.ports, { container: "", host: "", protocol: "tcp" }] })}>
            Add port
          </button>
        </Section>

        <Section title="Mounts">
          <p class="text-xs text-muted">
            Volumes keep data across deploys. Bind mounts reach into the server's file system, so they need the admin role and the destroy grant. Files are managed on the Files
            tab.
          </p>
          {f.mounts.map((m, i) => (
            <div key={i} class="grid items-end gap-2 sm:grid-cols-[7rem_1fr_1fr_auto_auto]">
              <Field label="Type">
                <Select
                  value={m.type}
                  options={[
                    ["volume", "Volume"],
                    ["bind", "Bind"],
                  ]}
                  onChange={(type) => mount(i, { type })}
                />
              </Field>
              {m.type === "bind" ? (
                <Input label="Server path" info="A folder or file on the server, mounted into the container as it is." class="font-mono" placeholder="/srv/data" value={m.source} onInput={(e) => mount(i, { source: e.currentTarget.value })} />
              ) : (
                <Input label="Volume name" info="A Docker volume, created on first use. Its data stays when the service is redeployed." class="font-mono" value={m.name} onInput={(e) => mount(i, { name: e.currentTarget.value })} />
              )}
              <Input label="Path in the container" info="Where the volume or folder appears inside the container." class="font-mono" value={m.target} onInput={(e) => mount(i, { target: e.currentTarget.value })} />
              <label class="flex items-center gap-2 pb-2">
                <input type="checkbox" checked={m.readonly} onChange={(e) => mount(i, { readonly: e.currentTarget.checked })} />
                Read-only
              </label>
              <IconButton icon="trash" label="Remove this mount" class="h-11 text-bad md:h-9" onClick={() => set({ mounts: removeAt(f.mounts, i) })} />
            </div>
          ))}
          {isDb && f.mounts.length > 0 && <p class="text-xs text-warn">Removing the data volume of a database loses its files on the next deploy.</p>}
          <button
            type="button"
            class="btn"
            onClick={() => set({ mounts: [...f.mounts, { type: "volume", name: `tp-${s.project}-${s.name}-data`, source: "", target: "", readonly: false }] })}
          >
            Add mount
          </button>
        </Section>

        <Section title="Container security">
          <Check
            checked={f.nnp}
            onChange={(nnp) => set({ nnp })}
            label="No new privileges"
            info="This stops setuid programs from gaining privileges inside the container, so leave it on unless the app needs sudo or something similar."
          />
          <Check checked={f.gpu} onChange={(gpu) => set({ gpu })} label="GPU access" info="The containers get access to the server's GPUs." />
          <div class="grid gap-4 sm:grid-cols-2">
            <TextArea label="Add capabilities" info="Write one per line, like NET_ADMIN; adding them needs the destroy grant." class="h-16" {...bind("capAdd")} />
            <TextArea label="Drop capabilities" info="Write one per line, like ALL." class="h-16" {...bind("capDrop")} />
          </div>
          <div class="space-y-3 rounded-md border border-bad/40 p-3">
            <p class="text-xs text-bad">These switches give the service control over the whole server. Turning them on needs the admin role and the destroy grant.</p>
            <Check
              checked={f.privileged}
              onChange={(privileged) => set({ privileged })}
              label="Privileged mode"
              hint={
                isCompose
                  ? "For a Compose stack this also allows host-level Compose options, such as bind mounts outside its folder, the Docker socket and host networking."
                  : undefined
              }
            />
            <Check checked={f.socket} onChange={(socket) => set({ socket })} label="Docker socket" info="The service can then start and stop any container on the server." />
            <Check
              checked={f.hostNet}
              onChange={(hostNet) => set({ hostNet })}
              label="Host networking"
              info="The service then shares the server's network stack instead of the project network."
            />
          </div>
        </Section>

        {f.srcType === "image" && !isDb && (
          <Section title="Image auto-update">
            <Check
              checked={f.auEnabled}
              onChange={(auEnabled) => set({ auEnabled })}
              label="Update the image automatically"
              info="New images roll out with health checks and roll back if they fail."
            />
            {f.auEnabled && (
              <div class="grid gap-4 sm:grid-cols-2">
                <Input label="Check every (minutes)" info="How often Tugport asks the registry for a newer image." type="number" min={5} {...bind("auInterval")} />
                <Field label="Follow" info="New images for the same tag follow tags like latest. Newer versions with the same major number move from 1.2 to 1.3 but never to 2.0.">
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

      {canEdit && dirty && <UnsavedBar s={sub} onDiscard={reset} />}
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
        <Field label="Region" info="Leave this empty to allow every region.">
          <input class="input font-mono" spellcheck={false} list="tp-regions" placeholder="Any region" value={f.region} onInput={(e) => set({ region: e.currentTarget.value })} />
          <datalist id="tp-regions">
            {regions.map((r) => (
              <option key={r} value={r} />
            ))}
          </datalist>
        </Field>
        <TextArea
          label="Labels"
          info="Write key=value pairs, separated by commas or new lines. Only servers with every one of these labels qualify."
          class="h-16"
          placeholder="disk=ssd"
          value={f.labels}
          onInput={(e) => set({ labels: e.currentTarget.value })}
        />
      </div>
      {!labels && <p class="text-sm text-bad">{LABELS_HINT}</p>}
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
        <Input
          label="Replicas"
          hint={
            kind === "database"
              ? "Databases run one primary, and read replicas have a tab of their own."
              : reasons.length
                ? "Every replica runs on the same server."
                : "Replicas spread over the matching servers, and each one gets traffic once it is healthy."
          }
          type="number"
          min={0}
          max={100}
          disabled={kind === "database"}
          value={f.replicas}
          onInput={(e) => set({ replicas: e.currentTarget.value })}
        />
      </PlacementFields>
      {reasons.length > 0 && props.moved && (
        <Note tone="warn">
          Its data stays on the server it runs on now. If that server stops matching these rules, the next deploy starts the service on another server with empty volumes.
        </Note>
      )}
      <p class="text-xs text-muted">
        {reasons.length > 0 && `This service stays on one server because ${and(reasons)}. `}
        Databases and services with volumes, bind mounts, published ports, host networking or the Docker socket stay on one server, because their data or ports live there. Other
        services spread their replicas over every online server that matches.
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
        <textarea
          class="input h-[60vh]"
          spellcheck={false}
          aria-label="Service spec as JSON"
          value={text}
          onInput={(e) => setText(e.currentTarget.value)}
          disabled={!atLeast(c.d.role, "operator")}
        />
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
