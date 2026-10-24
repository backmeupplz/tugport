import { useRef, useState } from "preact/hooks";
import { route } from "../router";
import { EnvEditor } from "../env-editor";
import { Icon, KindIcon } from "../icons";
import { NewService } from "./new-service";
import { ROLES, atLeast, del, hasGrant, patch, post, put, useApi, useMe, useNodes, type Event, type Project, type Role, type Source, type Status, type User } from "../api";
import { ServerSummary } from "./observe";
import { Action, Card, CardSkeleton, Empty, ErrorBox, Field, FormButtons, IconButton, Input, Loaded, LoadingPage, Modal, NameInput, Page, Pill, Rows, RowsSkeleton, Select, Skel, StateDot, toast, useForm, useStoredChoice, useSubmit } from "../ui";

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

/** Problems first, so they stand out on the project cards; unknown states go last. */
const STATE_ORDER = ["crashing", "unhealthy", "degraded", "stopped", "asleep", "not deployed", "running"];
const rank = (s: string) => (STATE_ORDER.includes(s) ? STATE_ORDER.indexOf(s) : STATE_ORDER.length);

/** A service as the full list (GET /services) returns it. */
interface ServiceItem {
  id: string;
  name: string;
  project: string;
  project_id: string;
  kind: string;
  state: string;
  running: number;
  desired: number;
}

const PROJECTS_LIVE = (e: Event) => e.kind === "service.status" || (e.kind === "audit" && /^(project|service)\./.test(e.data?.action ?? ""));

/** The dashboard: every server's numbers, then every project with its services. */
export function Projects() {
  const me = useMe();
  const r = useApi<{ projects: Project[] }>("/projects", PROJECTS_LIVE);
  const services = useApi<{ services: ServiceItem[] }>("/services", PROJECTS_LIVE);
  const nodes = useNodes();
  const [open, setOpen] = useState(false);
  const [layout, setLayout] = useStoredChoice<"grid" | "list">("tugport-projects", "grid");
  const canCreate = me.principal.is_owner || atLeast(me.principal.all_projects_role, "operator");
  const of = (p: Project) => services.data?.services.filter((x) => x.project_id === p.id);
  return (
    <Page
      title="Projects"
      actions={
        <>
          <div role="group" aria-label="Layout" class="flex rounded-lg border border-line p-0.5">
            {(
              [
                ["grid", "Grid"],
                ["list", "List"],
              ] as const
            ).map(([id, label]) => (
              <button
                key={id}
                type="button"
                aria-pressed={layout === id}
                aria-label={`Show as a ${label.toLowerCase()}`}
                title={label}
                class={`grid size-8 place-items-center rounded-md ${layout === id ? "bg-raised text-text" : "text-muted hover:text-text"}`}
                onClick={() => setLayout(id)}
              >
                <Icon name={id} />
              </button>
            ))}
          </div>
          {canCreate && (
            <button class="btn btn-primary" onClick={() => setOpen(true)}>
              New project
            </button>
          )}
        </>
      }
    >
      {nodes && nodes.nodes.length > 0 && (
        <section aria-label="Servers" class={`mb-8 grid gap-3 ${nodes.nodes.length > 1 ? "xl:grid-cols-2" : ""}`}>
          {nodes.nodes.map((n) => (
            <ServerSummary key={n.id} node={n} />
          ))}
        </section>
      )}
      <Loaded
        r={r}
        skeleton={
          layout === "list" ? (
            <CardSkeleton>
              <RowsSkeleton n={8} />
            </CardSkeleton>
          ) : (
            <ProjectsSkeleton />
          )
        }
      >
        {(d) =>
          d.projects.length === 0 ? (
            <Empty title="No projects yet">
              {canCreate
                ? "A project groups services that work together, and services in the same project reach each other by name on a private network."
                : "You do not have access to any project yet, so ask the owner to add you to one."}
            </Empty>
          ) : layout === "list" ? (
            <Card pad={false}>
              {d.projects.map((p) => (
                <ProjectRows key={p.id} project={p} services={of(p)} />
              ))}
            </Card>
          ) : (
            <div class="gap-3 sm:columns-2 xl:columns-3">
              {d.projects.map((p) => (
                <ProjectCard key={p.id} project={p} services={of(p)} />
              ))}
            </div>
          )
        }
      </Loaded>
      <NewProject open={open} onClose={() => setOpen(false)} />
    </Page>
  );
}

/** A role worth pointing out: anything short of admin limits what the card's links allow. */
const limitedRole = (role: Role | null) => (role && role !== "admin" ? role : null);

function ProjectCard(props: { project: Project; services?: ServiceItem[] }) {
  const p = props.project;
  const role = limitedRole(p.role);
  const list = sorted(props.services);
  return (
    <section class="card mb-3 break-inside-avoid">
      <a href={`/projects/${p.name}`} class="flex items-center gap-2 rounded-t-xl px-4 pt-3.5 pb-2 hover:text-accent-text">
        <h2 class="min-w-0 flex-1 truncate font-semibold">{p.name}</h2>
        {role && <span class="chip">{role}</span>}
        <span class="text-xs text-muted">{p.services === 1 ? "1 service" : `${p.services} services`}</span>
      </a>
      {p.description && <p class="line-clamp-2 px-4 pb-2 text-[13px] text-dim">{p.description}</p>}
      <ul class="pb-2">
        {!list
          ? Array.from({ length: Math.min(p.services, 4) }, (_, i) => (
              <li key={i} class="flex items-center gap-2.5 px-4 py-1.5">
                <Skel class="size-2 rounded-full" />
                <Skel class="h-3.5 w-32" />
              </li>
            ))
          : list.map((x) => (
              <li key={x.id}>
                <a href={`/projects/${p.name}/${x.name}`} class="flex items-center gap-2.5 px-4 py-2 text-sm hover:bg-raised/60 md:py-1.5 md:text-[13px]">
                  <StateDot state={x.state} />
                  <span class="min-w-0 flex-1 truncate">{x.name}</span>
                  {x.kind !== "app" && <span class="text-xs text-muted">{x.kind}</span>}
                  {x.state !== "running" && <span class="text-xs text-muted">{x.state}</span>}
                </a>
              </li>
            ))}
        {list?.length === 0 && <li class="px-4 py-1.5 text-[13px] text-muted">No services yet.</li>}
      </ul>
    </section>
  );
}

/** Problems first, so they stand out; then by name. */
const sorted = (list?: ServiceItem[]) => list && [...list].sort((a, b) => rank(a.state) - rank(b.state) || a.name.localeCompare(b.name));

/** One project in the list layout: a heading row, then a row per service. */
function ProjectRows(props: { project: Project; services?: ServiceItem[] }) {
  const p = props.project;
  const role = limitedRole(p.role);
  const list = sorted(props.services);
  return (
    <section class="border-b border-line last:border-b-0">
      <a href={`/projects/${p.name}`} class="flex items-center gap-2 bg-raised/40 px-4 py-2.5 hover:text-accent-text md:px-5">
        <h2 class="min-w-0 flex-1 truncate font-semibold">{p.name}</h2>
        {role && <span class="chip">{role}</span>}
        <span class="text-xs text-muted">{p.services === 1 ? "1 service" : `${p.services} services`}</span>
      </a>
      {list?.length === 0 && <p class="px-4 py-2 text-[13px] text-muted md:px-5">No services yet.</p>}
      <ul class="divide-y divide-line/60">
        {(list ?? []).map((x) => (
          <li key={x.id}>
            <a href={`/projects/${p.name}/${x.name}`} class="grid grid-cols-[auto_1fr_auto] items-center gap-x-3 px-4 py-2 text-sm hover:bg-raised/60 sm:grid-cols-[auto_1fr_7rem_8rem] md:px-5">
              <StateDot state={x.state} />
              <span class="min-w-0 truncate">{x.name}</span>
              <span class="hidden text-xs text-muted sm:block">{x.kind}</span>
              <span class="text-right text-xs text-muted">{x.state === "running" ? `${x.running} of ${x.desired} running` : x.state}</span>
            </a>
          </li>
        ))}
      </ul>
    </section>
  );
}

const ProjectsSkeleton = () => (
  <div class="gap-3 sm:columns-2 xl:columns-3" role="status" aria-label="Loading">
    {[3, 5, 2, 4, 3, 6].map((n, i) => (
      <div key={i} class="card mb-3 break-inside-avoid px-4 pt-4 pb-3" aria-hidden="true">
        <Skel class="mb-3 h-4 w-28" />
        {Array.from({ length: n }, (_, j) => (
          <div key={j} class="flex items-center gap-2.5 py-1.5">
            <Skel class="size-2 rounded-full" />
            <Skel class="h-3.5 w-36" />
          </div>
        ))}
      </div>
    ))}
  </div>
);

/** A project page before it arrives: its services list and the two cards under it. */
const ProjectSkeleton = () => (
  <div class="space-y-6" role="status" aria-label="Loading">
    <CardSkeleton>
      <RowsSkeleton n={4} />
    </CardSkeleton>
    <div class="grid gap-6 lg:grid-cols-2">
      <CardSkeleton />
      <CardSkeleton />
    </div>
  </div>
);

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
        <Input label="Description (optional)" {...bind("description")} />
        <FormButtons s={s} submit="Create project" onCancel={props.onClose} />
      </form>
    </Modal>
  );
}

export function ProjectPage(props: { project?: string }) {
  const me = useMe();
  const pid = useRef("");
  // Replicas on a server that drops out vanish from the status without a container event.
  const r = useApi<ProjectDetail>(`/projects/${props.project ?? ""}`, (e) => (e.project_id === pid.current && LIVE.has(e.kind)) || e.kind === "nodes");
  pid.current = r.data?.project.id ?? "";
  const [wizard, setWizard] = useState(false);
  const d = r.data;
  if (!d) return <LoadingPage title={props.project} crumbs={[["Projects", "/"]]} r={r} skeleton={<ProjectSkeleton />} />;
  const canOperate = atLeast(d.role, "operator");
  const isAdmin = atLeast(d.role, "admin");
  return (
    <Page
      title={d.project.name}
      crumbs={[["Projects", "/"]]}
      meta={limitedRole(d.role) && <span class="chip">your role: {d.role}</span>}
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
        <Rows
          items={d.services}
          emptyTitle="No services yet"
          empty={canOperate ? "Add an app from Git, a Docker image, a Dockerfile, a Compose stack or a database." : "Nobody has added a service to this project yet."}
        >
          {(s) => (
            <li key={s.id}>
              <a href={`/projects/${d.project.name}/${s.name}`} class="row flex-nowrap hover:bg-raised/60">
                <KindIcon kind={s.kind} />
                <div class="min-w-0 flex-1">
                  <div class="font-medium">{s.name}</div>
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
          )}
        </Rows>
      </Card>
      <div class="grid gap-6 lg:grid-cols-2">
        <Card title="Project variables">
          <p class="mb-4 text-dim">Every service in this project receives these variables. A service variable with the same name takes precedence.</p>
          <EnvEditor path={`/projects/${d.project.id}/vars`} field="vars" canEdit={canOperate} savedNote="Saved. Redeploy the services that use these variables to apply them." />
        </Card>
        <Members projectId={d.project.id} canManage={isAdmin} />
      </div>
      {isAdmin && hasGrant(me, "destroy") && (
        <Card title="Delete project" class="mt-6 border-bad/40">
          <p class="mb-3 text-dim">
            {d.services.length ? "A project can be deleted once all of its services are gone." : "This removes the empty project and its private network."}
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
  const edit = () => {
    setText(props.project.description ?? "");
    setEditing(true);
  };
  if (!props.project.description) {
    return props.canEdit ? (
      <button class="link mb-6 block text-sm" onClick={edit}>
        Add a description
      </button>
    ) : null;
  }
  return (
    <p class="mb-6 flex items-start gap-1 text-dim">
      <span class="min-w-0 py-1">{props.project.description}</span>
      {props.canEdit && <IconButton icon="edit" label="Edit the description" onClick={edit} />}
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
                        <Action class="btn btn-ghost btn-sm px-2 text-bad" label={`Remove ${m.name}`} confirm={`Remove ${m.name} from this project?`} run={() => setRole(m.id, null)}>
                          <Icon name="trash" />
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
                      onChange={(user) => setAdd((a) => ({ ...a, user }))}
                      options={[["", "Choose…"], ...candidates.map((u): [string, string] => [u.id, `${u.name} (${u.kind})`])]}
                    />
                  </Field>
                  <Field label="Role">
                    <Select value={add.role} onChange={(role) => setAdd((a) => ({ ...a, role }))} options={ROLES} />
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
