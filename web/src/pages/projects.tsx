import { useRef, useState } from "preact/hooks";
import { route } from "preact-router";
import { EnvEditor } from "../env-editor";
import { NewService } from "./new-service";
import {
  ROLES,
  atLeast,
  del,
  hasGrant,
  patch,
  post,
  put,
  useApi,
  useMe,
  type Project,
  type Role,
  type Source,
  type Status,
  type User,
} from "../api";
import { Action, Card, Empty, ErrorBox, Field, Loaded, Modal, NameInput, Page, Pill, Select, toast, useForm, useSubmit } from "../ui";

export interface ProjectDetail {
  project: { id: string; name: string; description: string | null; created_at: number };
  services: { id: string; name: string; kind: string; source: Source; status: Status }[];
  vars: string[];
  role: Role | null;
}

const LIVE = new Set(["deploy", "service.status", "service.updated", "audit"]);

export function sourceLabel(src: Source, kind?: string): string {
  switch (src.type) {
    case "git":
      return `${src.repo}${src.branch ? `@${src.branch}` : ""}${src.path ? ` · ${src.path}` : ""}`;
    case "image":
      return src.image;
    case "dockerfile":
      return "Inline Dockerfile";
    case "compose":
      return kind === "compose" ? "Pasted Compose file" : "Compose";
    default:
      return "";
  }
}

export function Projects(_: { path?: string }) {
  const me = useMe();
  const r = useApi<{ projects: Project[] }>("/projects", (e) => e.kind === "audit" && /^(project|service)\./.test(e.data?.action ?? ""));
  const [open, setOpen] = useState(false);
  const canCreate = me.principal.is_owner || atLeast(me.principal.all_projects_role, "operator");
  return (
    <Page
      title="Projects"
      actions={
        canCreate && (
          <button class="btn btn-primary" onClick={() => setOpen(true)}>
            New project
          </button>
        )
      }
    >
      <Loaded r={r}>
        {(d) =>
          d.projects.length === 0 ? (
            <Empty title="No projects yet">
              {canCreate
                ? "A project groups services that work together, and services in the same project reach each other by name on a private network."
                : "You do not have access to any project yet, so ask the owner to add you to one."}
            </Empty>
          ) : (
            <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {d.projects.map((p) => (
                <a key={p.id} href={`/projects/${p.name}`} class="card block p-4 hover:border-accent/60">
                  <div class="flex items-start justify-between gap-2">
                    <h2 class="min-w-0 truncate font-semibold">{p.name}</h2>
                    {p.role && <span class="chip">{p.role}</span>}
                  </div>
                  <p class="mt-1 line-clamp-2 min-h-10 text-dim">{p.description || "No description."}</p>
                  <p class="mt-3 text-xs text-muted">
                    {p.services} {p.services === 1 ? "service" : "services"}
                  </p>
                </a>
              ))}
            </div>
          )
        }
      </Loaded>
      <NewProject open={open} onClose={() => setOpen(false)} />
    </Page>
  );
}

function NewProject(props: { open: boolean; onClose: () => void }) {
  const { f, set, bind, reset } = useForm({ name: "", description: "" });
  const s = useSubmit(async () => {
    const r = await post<{ id: string; name: string }>("/projects", { name: f.name, description: f.description || undefined });
    reset();
    props.onClose();
    route(`/projects/${r.name}`);
  });
  return (
    <Modal open={props.open} onClose={props.onClose} title="New project">
      <form class="space-y-4" onSubmit={s.submit}>
        <Field label="Name" hint="Use lowercase letters, digits and dashes. The name becomes part of container and network names, so it cannot change later.">
          <NameInput value={f.name} onChange={(name) => set({ name })} />
        </Field>
        <Field label="Description (optional)">
          <input class="input" {...bind("description")} />
        </Field>
        <ErrorBox error={s.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={s.busy}>
            Create project
          </button>
        </div>
      </form>
    </Modal>
  );
}

export function ProjectPage(props: { project?: string; path?: string }) {
  const me = useMe();
  const pid = useRef("");
  const r = useApi<ProjectDetail>(`/projects/${props.project ?? ""}`, (e) => e.project_id === pid.current && LIVE.has(e.kind));
  pid.current = r.data?.project.id ?? "";
  const [wizard, setWizard] = useState(false);
  const d = r.data;
  if (!d) {
    return (
      <Page title={props.project}>
        <Loaded r={r}>{() => null}</Loaded>
      </Page>
    );
  }
  const canOperate = atLeast(d.role, "operator");
  const isAdmin = atLeast(d.role, "admin");
  return (
    <Page
      title={d.project.name}
      sub={
        <span>
          <a class="link" href="/">
            Projects
          </a>{" "}
          / {d.project.name}
          {d.role && <span class="chip ml-2">your role: {d.role}</span>}
        </span>
      }
      actions={
        canOperate && (
          <button class="btn btn-primary" onClick={() => setWizard(true)}>
            New service
          </button>
        )
      }
    >
      <Description project={d.project} canEdit={isAdmin} onSaved={r.reload} />
      <Card title="Services" pad={false} class="mb-6">
        {d.services.length === 0 ? (
          <div class="p-4">
            <Empty title="No services yet">
              {canOperate
                ? "Add an app from Git, a Docker image, a Dockerfile, a Compose stack or a database."
                : "Nobody has added a service to this project yet."}
            </Empty>
          </div>
        ) : (
          <ul class="divide-y divide-line">
            {d.services.map((s) => (
              <li key={s.id}>
                <a href={`/projects/${d.project.name}/${s.name}`} class="flex items-center gap-3 px-4 py-3 hover:bg-raised">
                  <div class="min-w-0 flex-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <span class="font-medium">{s.name}</span>
                      <span class="chip">{s.kind}</span>
                    </div>
                    <div class="mt-0.5 truncate font-mono text-xs text-muted">{sourceLabel(s.source, s.kind)}</div>
                  </div>
                  <div class="flex shrink-0 flex-col items-end gap-1">
                    <Pill state={s.status.state} />
                    {s.status.state !== "not deployed" && (
                      <span class="text-xs text-muted">
                        {s.status.running} of {s.status.desired} running
                      </span>
                    )}
                  </div>
                </a>
              </li>
            ))}
          </ul>
        )}
      </Card>
      <div class="grid gap-6 lg:grid-cols-2">
        <Card title="Project variables">
          <p class="mb-4 text-dim">
            Every service in this project receives these variables. A service variable with the same name takes precedence.
          </p>
          <EnvEditor path={`/projects/${d.project.id}/vars`} field="vars" canEdit={canOperate} savedNote="Saved. Redeploy the services that use these variables to apply them." />
        </Card>
        <Members projectId={d.project.id} canManage={isAdmin} />
      </div>
      {isAdmin && hasGrant(me, "destroy") && (
        <Card title="Delete project" class="mt-6 border-bad/40">
          <p class="mb-3 text-dim">
            {d.services.length
              ? "A project can be deleted once all of its services are gone."
              : "This removes the empty project and its private network."}
          </p>
          <Action
            class="btn btn-danger"
            disabled={d.services.length > 0}
            confirm={`Delete the project ${d.project.name}?`}
            run={async () => {
              await del(`/projects/${d.project.id}`);
              toast(`Deleted ${d.project.name}.`);
              route("/");
            }}
          >
            Delete project
          </Action>
        </Card>
      )}
      <NewService project={d.project} open={wizard} onClose={() => setWizard(false)} onCreated={r.reload} />
    </Page>
  );
}

function Description(props: { project: ProjectDetail["project"]; canEdit: boolean; onSaved: () => void }) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState("");
  const s = useSubmit(async () => {
    await patch(`/projects/${props.project.id}`, { description: text });
    setEditing(false);
    props.onSaved();
  });
  if (editing) {
    return (
      <form class="mb-6 flex flex-wrap gap-2" onSubmit={s.submit}>
        <input class="input flex-1" aria-label="Description" value={text} onInput={(e) => setText(e.currentTarget.value)} autoFocus />
        <button class="btn btn-primary" disabled={s.busy}>
          Save
        </button>
        <button type="button" class="btn" onClick={() => setEditing(false)}>
          Cancel
        </button>
        <ErrorBox error={s.error} />
      </form>
    );
  }
  return (
    <p class="mb-6 text-dim">
      {props.project.description || "No description."}{" "}
      {props.canEdit && (
        <button
          class="link"
          onClick={() => {
            setText(props.project.description ?? "");
            setEditing(true);
          }}
        >
          Edit
        </button>
      )}
    </p>
  );
}

interface Member {
  id: string;
  name: string;
  kind: string;
  role: string | null;
  via_all_projects: boolean;
}

function Members(props: { projectId: string; canManage: boolean }) {
  const r = useApi<{ members: Member[] }>(`/projects/${props.projectId}/members`);
  const users = useApi<{ users: User[] }>(props.canManage ? "/users" : null);
  const [add, setAdd] = useState({ user: "", role: "viewer" });
  const setRole = (user: string, role: string | null) => put(`/projects/${props.projectId}/members/${user}`, { role }).then(r.reload);
  const s = useSubmit(async () => {
    await setRole(add.user, add.role);
    setAdd({ user: "", role: "viewer" });
  });
  return (
    <Card title="Members">
      <Loaded r={r}>
        {(d) => {
          const explicit = new Set(d.members.filter((m) => !m.via_all_projects).map((m) => m.id));
          const candidates = (users.data?.users ?? []).filter((u) => !u.is_owner && !explicit.has(u.id));
          return (
            <>
              <ul class="divide-y divide-line">
                {d.members.map((m) => (
                  <li key={m.id} class="flex flex-wrap items-center gap-2 py-2">
                    <span class="min-w-0 flex-1 truncate">
                      <span class="font-medium">{m.name}</span> <span class="chip">{m.kind}</span>
                      {m.via_all_projects && m.role !== "owner" && <span class="ml-1 text-xs text-muted">through access to all projects</span>}
                    </span>
                    {props.canManage && m.role !== "owner" && !m.via_all_projects ? (
                      <>
                        <Select
                          class="w-auto"
                          aria-label={`Role of ${m.name}`}
                          value={m.role ?? "viewer"}
                          options={ROLES}
                          onChange={(v) => setRole(m.id, v).catch((e) => toast(String(e.message), "bad"))}
                        />
                        <Action class="btn btn-ghost text-bad" confirm={`Remove ${m.name} from this project?`} run={() => setRole(m.id, null)}>
                          Remove
                        </Action>
                      </>
                    ) : (
                      <span class="chip">{m.role}</span>
                    )}
                  </li>
                ))}
              </ul>
              {props.canManage && candidates.length > 0 && (
                <form class="mt-4 flex flex-wrap items-end gap-2 border-t border-line pt-4" onSubmit={s.submit}>
                  <Field label="Add a person or agent" class="min-w-40 flex-1">
                    <Select
                      value={add.user}
                      onChange={(v) => setAdd({ ...add, user: v })}
                      options={[["", "Choose…"], ...candidates.map((u): [string, string] => [u.id, `${u.name} (${u.kind})`])]}
                    />
                  </Field>
                  <Field label="Role">
                    <Select value={add.role} onChange={(v) => setAdd({ ...add, role: v })} options={ROLES} />
                  </Field>
                  <button class="btn" disabled={!add.user || s.busy}>
                    Add
                  </button>
                  <ErrorBox error={s.error} />
                </form>
              )}
            </>
          );
        }}
      </Loaded>
    </Card>
  );
}
