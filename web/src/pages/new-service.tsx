import { useState } from "preact/hooks";
import { post, useApi } from "../api";
import { Action, Check, CodeBlock, ErrorBox, Field, Input, Modal, NameInput, Note, Select, TextArea, errText, useForm, useSubmit } from "../ui";
import { lines } from "../util";

type Kind = "git" | "image" | "dockerfile" | "compose" | "database";
const TYPES: [Kind, string, string][] = [
  ["git", "App from Git", "Builds a repository and redeploys it when you push."],
  ["image", "Docker image", "Runs an image from Docker Hub or another registry."],
  ["dockerfile", "Dockerfile", "Builds a Dockerfile that you paste here."],
  ["compose", "Compose stack", "Runs a Docker Compose file, pasted or from a repository."],
  ["database", "Database", "Postgres, MySQL, MariaDB, MongoDB or Redis with generated credentials."],
];

export const PROVIDER_KINDS: Record<string, string> = {
  github_app: "GitHub App",
  github_token: "GitHub token",
  forgejo: "Forgejo",
  gitea: "Gitea",
  gitlab: "GitLab",
};

/** Where a repository comes from: a URL or one of the connected Git hosts. */
export const providerOptions = (list: { id: string; kind: string; name: string }[] = []): [string, string][] => [
  ["", "A URL (public, or private with a deploy key)"],
  ...list.map((p): [string, string] => [p.id, `${p.name} (${PROVIDER_KINDS[p.kind] ?? p.kind})`]),
];

export const BUILDS: [string, string][] = [
  ["auto", "Automatic (Dockerfile if present, otherwise Railpack)"],
  ["dockerfile", "Dockerfile"],
  ["railpack", "Railpack"],
  ["nixpacks", "Nixpacks"],
  ["buildpacks", "Buildpacks"],
];

const ENGINES: [string, string][] = [
  ["postgres", "PostgreSQL"],
  ["mysql", "MySQL"],
  ["mariadb", "MariaDB"],
  ["mongo", "MongoDB"],
  ["redis", "Redis"],
];

const INIT = {
  name: "",
  provider: "",
  repo: "",
  branch: "main",
  path: "",
  build: "auto",
  dockerfile: "Dockerfile",
  target: "",
  builder: "heroku/builder:24",
  watch: "",
  auto: true,
  image: "",
  content: "",
  composeFrom: "paste",
  yaml: "",
  file: "docker-compose.yml",
  engine: "postgres",
  dbImage: "",
  dbUser: "",
  dbName: "",
  deploy: true,
};
type Form = typeof INIT;

interface Created {
  id: string;
  name: string;
  deploy_hook?: string;
  database?: { url: string; project_var: string } | null;
  deployId?: string;
  deployError?: string;
}

const needsKey = (f: Form) => !f.provider && /^(git@|ssh:\/\/)/.test(f.repo.trim());

export function gitSource(f: Pick<Form, "provider" | "repo" | "branch" | "path" | "watch" | "auto">, compose = false) {
  return {
    type: "git" as const,
    provider: f.provider || null,
    repo: f.repo.trim(),
    branch: f.branch.trim() || "main",
    path: compose ? "" : f.path.trim(),
    watch_paths: compose ? [] : lines(f.watch),
    auto_deploy: f.auto,
  };
}

type BuildForm = Pick<Form, "build" | "dockerfile" | "target" | "builder">;

export function buildSpec(f: BuildForm) {
  if (f.build === "dockerfile") return { type: "dockerfile", path: f.dockerfile.trim() || "Dockerfile", target: f.target.trim() || null };
  if (f.build === "buildpacks") return { type: "buildpacks", builder: f.builder.trim() || "heroku/builder:24" };
  return { type: f.build };
}

function requestBody(kind: Kind, f: Form) {
  const name = f.name;
  switch (kind) {
    case "git":
      return { name, kind: "app", spec: { source: gitSource(f), build: buildSpec(f) } };
    case "image":
      return { name, kind: "app", spec: { source: { type: "image", image: f.image.trim() } } };
    case "dockerfile":
      return { name, kind: "app", spec: { source: { type: "dockerfile", content: f.content } } };
    case "compose":
      return f.composeFrom === "paste"
        ? { name, kind: "compose", spec: { source: { type: "compose", yaml: f.yaml } } }
        : { name, kind: "compose", spec: { source: gitSource(f, true), compose: { file: f.file.trim() || "docker-compose.yml" } } };
    case "database":
      return {
        name,
        kind: "database",
        database: { engine: f.engine, user: f.dbUser.trim() || undefined, database: f.dbName.trim() || undefined },
        spec: f.dbImage.trim() ? { source: { type: "image", image: f.dbImage.trim() } } : undefined,
      };
  }
}

export function NewService(props: { project: { id: string; name: string }; open: boolean; onClose: () => void; onCreated: () => void }) {
  const [kind, setKind] = useState<Kind | null>(null);
  const [created, setCreated] = useState<Created | null>(null);
  const form = useForm(INIT);
  const { f, set, bind } = form;
  const close = () => {
    props.onClose();
    setKind(null);
    setCreated(null);
    form.reset();
  };
  const s = useSubmit(async () => {
    if (!kind) return;
    const res = await post<Created>(`/projects/${props.project.id}/services`, requestBody(kind, f));
    if (f.deploy && !(kind === "git" && needsKey(f))) {
      try {
        res.deployId = (await post<{ deploy_id: string }>(`/services/${res.id}/deploy`, {})).deploy_id;
      } catch (e) {
        res.deployError = errText(e);
      }
    }
    setCreated(res);
    props.onCreated();
  });
  const title = created ? `${created.name} is ready` : kind ? `New service: ${TYPES.find((t) => t[0] === kind)?.[1]}` : "New service";

  return (
    <Modal open={props.open} onClose={close} title={title} wide>
      {created ? (
        <Result created={created} project={props.project.name} gitKey={kind === "git" && needsKey(f)} onClose={close} />
      ) : !kind ? (
        <div class="grid gap-3 sm:grid-cols-2">
          {TYPES.map(([k, label, help]) => (
            <button key={k} type="button" class="card p-4 text-left hover:border-accent/60" onClick={() => setKind(k)}>
              <span class="block font-semibold">{label}</span>
              <span class="mt-1 block text-dim">{help}</span>
            </button>
          ))}
        </div>
      ) : (
        <form class="space-y-4" onSubmit={s.submit}>
          <Field label="Service name" hint="Other services in this project reach it by this name.">
            <NameInput value={f.name} onChange={(name) => set({ name })} />
          </Field>

          {kind === "git" && (
            <>
              <GitFields form={form} />
              <BuildFields f={f} set={set} bind={bind} />
              <TextArea
                label="Watch paths (optional)"
                hint="Redeploy only when a push changes these paths, one glob per line. Leave this empty to redeploy on any change."
                class="h-20"
                placeholder={"apps/api/**\npackages/shared/**"}
                {...bind("watch")}
              />
              <Check checked={f.auto} onChange={(auto) => set({ auto })} label="Deploy on every push" hint="Pushes to the branch start a deploy automatically." />
            </>
          )}

          {kind === "image" && (
            <Input
              label="Image"
              hint="Registry credentials for private images are set once in Settings, under Registries."
              mono
              required
              placeholder="ghcr.io/owner/app:1.4.2"
              {...bind("image")}
            />
          )}

          {kind === "dockerfile" && (
            <TextArea label="Dockerfile" class="h-56" required placeholder={'FROM node:24-alpine\nWORKDIR /app\nCOPY . .\nCMD ["node", "server.js"]'} {...bind("content")} />
          )}

          {kind === "compose" && (
            <>
              <fieldset class="flex flex-wrap gap-4">
                <legend class="label">Compose file from</legend>
                {[
                  ["paste", "Paste YAML"],
                  ["repo", "A repository"],
                ].map(([v, l]) => (
                  <label key={v} class="flex items-center gap-2">
                    <input type="radio" name="composeFrom" checked={f.composeFrom === v} onChange={() => set({ composeFrom: v })} />
                    {l}
                  </label>
                ))}
              </fieldset>
              {f.composeFrom === "paste" ? (
                <TextArea
                  label="docker-compose.yml"
                  hint="Every container shows up with its own logs, charts and domains. Sections with build: use the same builder as apps."
                  class="h-64"
                  required
                  placeholder={"services:\n  web:\n    image: nginx:alpine"}
                  {...bind("yaml")}
                />
              ) : (
                <GitFields form={form} compose />
              )}
            </>
          )}

          {kind === "database" && (
            <>
              <div class="grid gap-4 sm:grid-cols-2">
                <Field label="Engine">
                  <Select value={f.engine} onChange={(engine) => set({ engine })} options={ENGINES} />
                </Field>
                <Input label="Image (optional)" hint="Leave this empty to pin the newest stable release." mono placeholder={`${f.engine}:<version>`} {...bind("dbImage")} />
              </div>
              {f.engine !== "redis" && (
                <details>
                  <summary class="text-sm text-dim">User and database name</summary>
                  <div class="mt-3 grid gap-4 sm:grid-cols-2">
                    <Input label="User" class="font-mono" placeholder={f.engine === "mongo" ? "admin" : "app"} {...bind("dbUser")} />
                    <Input label="Database" class="font-mono" placeholder="app" {...bind("dbName")} />
                  </div>
                </details>
              )}
              <p class="text-xs text-muted">Tugport generates the password, keeps the data in a volume and adds a connection URL to the project variables.</p>
            </>
          )}

          <Check
            checked={f.deploy && !(kind === "git" && needsKey(f))}
            disabled={kind === "git" && needsKey(f)}
            onChange={(deploy) => set({ deploy })}
            label="Deploy right away"
            hint={kind === "git" && needsKey(f) ? "Add the deploy key to the repository first; the next step shows it." : undefined}
          />
          <ErrorBox error={s.error} />
          <div class="flex justify-between gap-2">
            <button type="button" class="btn" onClick={() => setKind(null)}>
              Back
            </button>
            <button class="btn btn-primary" disabled={s.busy}>
              {s.busy ? "Creating…" : "Create service"}
            </button>
          </div>
        </form>
      )}
    </Modal>
  );
}

function GitFields(props: { form: ReturnType<typeof useForm<Form>>; compose?: boolean }) {
  const { f, set, bind } = props.form;
  const providers = useApi<{ providers: { id: string; kind: string; name: string }[] }>("/git/providers");
  const repos = useApi<{ repos: string[] }>(f.provider ? `/git/providers/${f.provider}/repos` : null);
  const list = providers.data?.providers ?? [];
  return (
    <>
      <Field label="Repository from" hint={list.length ? undefined : "Connect GitHub, Forgejo, Gitea or GitLab in Settings to pick repositories and deploy on push."}>
        <Select value={f.provider} onChange={(provider) => set({ provider, repo: "" })} options={providerOptions(list)} />
      </Field>
      {f.provider ? (
        <Field
          label="Repository"
          hint={
            repos.error
              ? `Could not list repositories: ${repos.error.message}`
              : repos.loading
                ? "Loading repositories…"
                : "Start typing to search the repositories this connection can see."
          }
        >
          <input class="input font-mono" required list="tp-repos" placeholder="owner/name" spellcheck={false} autoComplete="off" {...bind("repo")} />
          <datalist id="tp-repos">
            {(repos.data?.repos ?? []).map((r) => (
              <option key={r} value={r} />
            ))}
          </datalist>
        </Field>
      ) : (
        <Input
          label="Repository URL"
          hint="A private repository needs its SSH URL, like git@github.com:owner/app.git, and the deploy key that the next step shows."
          mono
          required
          placeholder="https://github.com/owner/app.git"
          autoComplete="off"
          {...bind("repo")}
        />
      )}
      <div class="grid gap-4 sm:grid-cols-2">
        <Input label="Branch" mono required {...bind("branch")} />
        {props.compose ? (
          <Input label="Compose file" mono {...bind("file")} />
        ) : (
          <Input label="Path (optional)" hint="This is the build context inside the repository." mono placeholder="apps/api" {...bind("path")} />
        )}
      </div>
    </>
  );
}

/** The build method and its options, shared by the new-service form and the service settings. */
export function BuildFields(props: { f: BuildForm; set: (p: Partial<BuildForm>) => void; bind: (k: keyof BuildForm) => { value: string; onInput: (e: any) => void } }) {
  const { f, set, bind } = props;
  return (
    <>
      <Field label="Build method">
        <Select value={f.build} onChange={(build) => set({ build })} options={BUILDS} />
      </Field>
      {f.build === "dockerfile" && (
        <div class="grid gap-4 sm:grid-cols-2">
          <Input label="Dockerfile path" mono {...bind("dockerfile")} />
          <Input label="Target stage (optional)" mono {...bind("target")} />
        </div>
      )}
      {f.build === "buildpacks" && <Input label="Builder image" mono {...bind("builder")} />}
    </>
  );
}

function Result(props: { created: Created; project: string; gitKey: boolean; onClose: () => void }) {
  const c = props.created;
  return (
    <div class="space-y-4">
      <Note tone="ok">
        {c.name} was created{c.deployId ? ", and its first deploy has started" : ""}.
      </Note>
      {c.deployError && <ErrorBox error={`The service was created, but the deploy did not start: ${c.deployError}`} />}
      {c.database && (
        <div class="space-y-2">
          <CodeBlock label="Connection URL" text={c.database.url} />
          <p class="text-xs text-muted">
            Copy it now, because this is the only time it is shown here. It is also saved as the project variable <code>{c.database.project_var}</code>, so other services in the
            project can use it.
          </p>
        </div>
      )}
      {c.deploy_hook && (
        <div class="space-y-2">
          <CodeBlock label="Deploy webhook" text={c.deploy_hook} />
          <p class="text-xs text-muted">
            A POST or GET to this URL deploys the service, which suits CI pipelines and image pushes. It is shown only now; you can rotate it on the service page to get a new one.
          </p>
        </div>
      )}
      {props.gitKey && (
        <div class="space-y-2">
          <p class="text-dim">The repository URL uses SSH, so Tugport needs a deploy key to read it.</p>
          <DeployKey serviceId={c.id} />
        </div>
      )}
      <div class="flex justify-end gap-2">
        <button type="button" class="btn" onClick={props.onClose}>
          Close
        </button>
        <a class="btn btn-primary" href={`/projects/${props.project}/${c.name}`} onClick={props.onClose}>
          Open service
        </a>
      </div>
    </div>
  );
}

/** Generates an SSH key pair for a service and shows the public half. */
export function DeployKey(props: { serviceId: string }) {
  const [key, setKey] = useState("");
  if (key) {
    return (
      <div class="space-y-2">
        <CodeBlock label="Public deploy key" text={key} />
        <p class="text-xs text-muted">Add this key to the repository as a read-only deploy key. Generating another key replaces this one.</p>
      </div>
    );
  }
  return <Action run={async () => setKey((await post<{ public_key: string }>(`/services/${props.serviceId}/deploy-key`)).public_key)}>Generate deploy key</Action>;
}
