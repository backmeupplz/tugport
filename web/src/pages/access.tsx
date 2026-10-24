import { useEffect, useRef, useState } from "preact/hooks";
import { ROLES, ROLE_HELP, del, get, patch, post, useApi, useEvent, useMe, useProjects, type Key, type Project, type Role, type Snippets, type User } from "../api";
import { route } from "../router";
import {
  Action,
  Card,
  CodeBlock,
  Empty,
  ErrorBox,
  Field,
  FormButtons,
  Input,
  Loaded,
  Loading,
  LoadingPage,
  Modal,
  Note,
  Page,
  Rows,
  RowsSkeleton,
  Select,
  Tabs,
  TextArea,
  toast,
  useForm,
  useSubmit,
} from "../ui";
import { ago, cidr, qs, when, words } from "../util";

// ---- Access editing shared by new agents, new people and existing users ----

interface Access {
  mode: "all" | "some";
  allRole: Role;
  projects: Record<string, Role | "">;
  secrets: boolean;
  destroy: boolean;
}

const toAccess = (u: User): Access => ({
  mode: u.all_projects_role ? "all" : "some",
  allRole: u.all_projects_role ?? "deployer",
  projects: { ...(u.projects ?? {}) },
  secrets: !!u.grants?.includes("secrets:read"),
  destroy: !!u.grants?.includes("destroy"),
});

function accessBody(a: Access) {
  return {
    all_projects_role: a.mode === "all" ? a.allRole : null,
    projects: a.mode === "some" ? Object.fromEntries(Object.entries(a.projects).filter(([, r]) => r)) : {},
    grants: [...(a.secrets ? ["secrets:read"] : []), ...(a.destroy ? ["destroy"] : [])],
  };
}

function AccessEditor(props: { value: Access; onChange: (a: Access) => void; projects: Project[]; who: string }) {
  const a = props.value;
  const set = (p: Partial<Access>) => props.onChange({ ...a, ...p });
  return (
    <div class="space-y-5">
      <fieldset class="space-y-3">
        <legend class="label">Projects</legend>
        <label class="flex items-start gap-2.5">
          <input type="radio" class="mt-1" name="access-mode" checked={a.mode === "all"} onChange={() => set({ mode: "all" })} />
          <span class="min-w-0 flex-1">
            <span class="font-medium">All projects, including ones created later</span>
            {a.mode === "all" && (
              <span class="mt-2 block max-w-xs">
                <Select aria-label="Role on all projects" value={a.allRole} onChange={(v) => set({ allRole: v as Role })} options={ROLES} />
                <span class="hint">{ROLE_HELP[a.allRole]}</span>
              </span>
            )}
          </span>
        </label>
        <label class="flex items-start gap-2.5">
          <input type="radio" class="mt-1" name="access-mode" checked={a.mode === "some"} onChange={() => set({ mode: "some" })} />
          <span class="font-medium">Only the projects chosen below</span>
        </label>
        {a.mode === "some" &&
          (props.projects.length === 0 ? (
            <p class="ml-7 text-muted">There are no projects yet, so give access to all projects instead.</p>
          ) : (
            <ul class="ml-7 inset-list">
              {props.projects.map((p) => (
                <li key={p.id} class="flex flex-wrap items-center justify-between gap-2 px-3 py-2">
                  <span class="font-medium">{p.name}</span>
                  <Select
                    class="w-auto"
                    aria-label={`Role on ${p.name}`}
                    value={a.projects[p.id] ?? ""}
                    onChange={(v) => set({ projects: { ...a.projects, [p.id]: v as Role | "" } })}
                    options={[["", "No access"], ...ROLES.map((r): [string, string] => [r, r])]}
                  />
                </li>
              ))}
            </ul>
          ))}
        {a.mode === "some" && (
          <ul class="ml-7 space-y-0.5 text-xs text-muted">
            {ROLES.map((r) => (
              <li key={r}>{ROLE_HELP[r]}</li>
            ))}
          </ul>
        )}
      </fieldset>

      <fieldset class="space-y-3">
        <legend class="label">Extra grants</legend>
        <p class="text-xs text-muted">No role includes these, so add them only when {props.who} needs them.</p>
        <label class="flex items-start gap-2.5">
          <input type="checkbox" class="mt-1 size-4" checked={a.secrets} onChange={(e) => set({ secrets: e.currentTarget.checked })} />
          <span>
            <span class="font-medium">Read secrets</span> <code class="text-xs text-muted">secrets:read</code>
            <span class="hint">The grant lets {props.who} read environment values and files back, while writing them never needs it.</span>
          </span>
        </label>
        {a.secrets && (
          <Note tone="warn" class="ml-7">
            With this grant, {props.who} can read every secret in the projects where it has the operator role or higher, including database passwords and API keys.
          </Note>
        )}
        <label class="flex items-start gap-2.5">
          <input type="checkbox" class="mt-1 size-4" checked={a.destroy} onChange={(e) => set({ destroy: e.currentTarget.checked })} />
          <span>
            <span class="font-medium">Destroy</span> <code class="text-xs text-muted">destroy</code>
            <span class="hint">
              The grant lets an admin delete services, volumes and databases, and turn on privileged mode, the Docker socket, host networking, bind mounts and extra capabilities.
            </span>
          </span>
        </label>
        {a.destroy && (
          <Note tone="bad" class="ml-7">
            With this grant and the admin role, {props.who} can delete data for good and take control of the whole server, so give it only when you fully trust it.
          </Note>
        )}
      </fieldset>
    </div>
  );
}

const EXPIRY: [string, string][] = [
  ["7", "In 7 days"],
  ["30", "In 30 days"],
  ["90", "In 90 days"],
  ["365", "In a year"],
  ["", "Never"],
];

type KeyForm = { expires: string; ips: string };
const keyBody = (f: KeyForm) => ({ expires_days: f.expires ? Number(f.expires) : undefined, allowed_ips: words(f.ips).map(cidr) });

function KeyFields(props: { f: KeyForm; set: (p: Partial<KeyForm>) => void }) {
  return (
    <div class="grid gap-4 sm:grid-cols-2">
      <Field label="Key expires">
        <Select value={props.f.expires} onChange={(expires) => props.set({ expires })} options={EXPIRY} />
      </Field>
      <TextArea
        label="Allowed addresses (optional)"
        hint="Write addresses or ranges separated by commas or new lines, like 203.0.113.7 or 10.0.0.0/8, or leave this empty to allow any address."
        class="h-16"
        value={props.f.ips}
        onInput={(e) => props.set({ ips: e.currentTarget.value })}
      />
    </div>
  );
}

const SNIPPETS: [keyof Snippets, string][] = [
  ["env", "Environment variables"],
  ["cli", "Tugport CLI"],
  ["claude_code", "Claude Code"],
  ["codex", "Codex"],
  ["curl", "curl"],
];

/** A new token, shown once, with ready-to-paste ways to use it. */
function TokenReveal(props: { token: string; snippets: Snippets }) {
  return (
    <div class="space-y-4">
      <Note tone="warn">
        <strong>Store this token now.</strong> It is shown only this once and cannot be recovered later. If it gets lost, rotate the key to get a new one.
      </Note>
      <CodeBlock label="API token" text={props.token} />
      <h3 class="pt-2 font-semibold">Use it with</h3>
      <div class="grid gap-3 lg:grid-cols-2">
        {SNIPPETS.filter(([k]) => props.snippets[k]).map(([k, label]) => (
          <CodeBlock key={k} label={label} text={props.snippets[k]} />
        ))}
      </div>
    </div>
  );
}

function AccessTabs(props: { current: string }) {
  const me = useMe();
  return (
    <Tabs
      label="Access sections"
      current={props.current}
      items={[
        ["people", me.principal.is_owner ? "People and agents" : "Your keys", "/access"],
        ["audit", "Audit log", "/access/audit"],
      ]}
    />
  );
}

// ---- Pages ----

export function Access() {
  const me = useMe();
  // Only the owner manages other users; everyone else manages their own keys here.
  return me.principal.is_owner ? <People /> : <UserPage id={me.principal.user_id} />;
}

function People() {
  const users = useApi<{ users: User[] }>("/users", (e) => e.kind === "audit" && /^(user|agent|key)\./.test(e.data?.action ?? ""));
  const projects = useProjects();
  const names = Object.fromEntries((projects.data?.projects ?? []).map((p) => [p.id, p.name]));
  const summary = (u: User) =>
    u.is_owner
      ? "Owner, with full access"
      : u.all_projects_role
        ? `${u.all_projects_role} on all projects`
        : Object.entries(u.projects ?? {})
            .map(([pid, r]) => `${names[pid] ?? pid}: ${r}`)
            .join(", ") || "No project access";
  return (
    <Page
      title="Access"
      actions={
        <>
          <a class="btn" href="/access/new-person">
            New person
          </a>
          <a class="btn btn-primary" href="/access/new-agent">
            New agent
          </a>
        </>
      }
    >
      <AccessTabs current="people" />
      <Loaded r={users}>
        {(d) => (
          <ul class="card divide-y divide-line">
            {d.users.map((u) => (
              <li key={u.id}>
                <a href={`/access/users/${u.id}`} class="row gap-y-1 hover:bg-raised/60">
                  <span class="flex min-w-0 flex-1 flex-wrap items-center gap-2">
                    <span class="font-medium">{u.name}</span>
                    <span class="chip">{u.kind}</span>
                    {u.disabled && <span class="chip text-bad">disabled</span>}
                    {(u.grants ?? []).map((g) => (
                      <span key={g} class="chip text-warn">
                        {g}
                      </span>
                    ))}
                  </span>
                  <span class="w-full truncate text-dim sm:w-auto sm:max-w-[50%]">{summary(u)}</span>
                  {u.kind === "human" && <span class="text-xs text-muted">{u.totp ? "2FA on" : "2FA off"}</span>}
                </a>
              </li>
            ))}
          </ul>
        )}
      </Loaded>
    </Page>
  );
}

export function NewAgent() {
  const me = useMe();
  const projects = useProjects();
  const { f, set, bind } = useForm({ name: "", description: "", expires: "90", ips: "" });
  const [access, setAccess] = useState<Access>({ mode: "all", allRole: "deployer", projects: {}, secrets: false, destroy: false });
  const [result, setResult] = useState<{ user: User; key: { id: string; token: string }; snippets: Snippets } | null>(null);
  const s = useSubmit(async () => {
    const body = accessBody(access);
    if (access.mode === "some" && Object.keys(body.projects).length === 0) throw new Error("Choose at least one project, or give the agent a role on all projects.");
    setResult(await post("/agents", { name: f.name.trim(), description: f.description.trim() || undefined, ...body, key: keyBody(f) }));
    window.scrollTo(0, 0);
  });

  if (!me.principal.is_owner) {
    return (
      <Page title="New agent">
        <Note>Only the owner can create agents.</Note>
      </Page>
    );
  }
  if (result) {
    return (
      <Page
        title={`${result.user.name} is ready`}
        sub="The agent can use the API with this token right away."
        actions={
          <a class="btn" href={`/access/users/${result.user.id}`}>
            View agent
          </a>
        }
      >
        <TokenReveal token={result.key.token} snippets={result.snippets} />
      </Page>
    );
  }
  return (
    <Page title="New agent" sub="An agent has no password and cannot open this panel. It works only through API keys, with exactly the access you choose here.">
      <form class="max-w-3xl space-y-6" onSubmit={s.submit}>
        <Card title="Agent">
          <div class="grid gap-4 sm:grid-cols-2">
            <Input label="Name" required autoFocus maxLength={64} placeholder="ci-deployer" {...bind("name")} />
            <Input label="Description (optional)" placeholder="Deploys from GitHub Actions" {...bind("description")} />
          </div>
        </Card>
        <Card title="Access">{projects.data ? <AccessEditor value={access} onChange={setAccess} projects={projects.data.projects} who="the agent" /> : <Loading />}</Card>
        <Card title="Key">
          <KeyFields f={f} set={set} />
        </Card>
        <FormButtons s={s} submit="Create agent and key" cancelHref="/access" />
      </form>
    </Page>
  );
}

export function NewPerson() {
  const projects = useProjects();
  const { f, bind } = useForm({ name: "", email: "", password: "" });
  const [access, setAccess] = useState<Access>({ mode: "some", allRole: "viewer", projects: {}, secrets: false, destroy: false });
  const s = useSubmit(async () => {
    const u = await post<User>("/users", { kind: "human", name: f.name.trim(), email: f.email.trim() || undefined, password: f.password, ...accessBody(access) });
    toast(`${u.name} can sign in now.`);
    route(`/access/users/${u.id}`);
  });
  return (
    <Page title="New person" sub="People sign in to this panel with a password and can turn on two-factor authentication.">
      <form class="max-w-3xl space-y-6" onSubmit={s.submit}>
        <Card title="Person">
          <div class="grid gap-4 sm:grid-cols-2">
            <Input label="Name" required autoFocus maxLength={64} autoComplete="off" {...bind("name")} />
            <Input label="Email (optional)" type="email" autoComplete="off" {...bind("email")} />
            <Input
              label="Password"
              hint="Use at least 10 characters, and share it through a safe channel."
              type="password"
              required
              minLength={10}
              autoComplete="new-password"
              {...bind("password")}
            />
          </div>
        </Card>
        <Card title="Access">{projects.data ? <AccessEditor value={access} onChange={setAccess} projects={projects.data.projects} who="this person" /> : <Loading />}</Card>
        <FormButtons s={s} submit="Create person" cancelHref="/access" />
      </form>
    </Page>
  );
}

export function UserPage(props: { id?: string }) {
  const me = useMe();
  const id = props.id ?? "";
  const owner = me.principal.is_owner;
  const self = id === me.principal.user_id;
  const r = useApi<User>(`/users/${id}`);
  const projects = useProjects(owner);
  const u = r.data;
  if (!u) return <LoadingPage title="Access" r={r} />;
  const editable = owner && !u.is_owner;
  return (
    <Page
      title={u.name}
      crumbs={owner ? [["Access", "/access"]] : undefined}
      meta={
        <>
          <span class="chip">{u.kind}</span>
          {u.is_owner && <span class="chip">owner</span>}
          {u.disabled && <span class="chip text-bad">disabled</span>}
        </>
      }
      sub={u.description}
      actions={
        editable && (
          <>
            <Action
              confirm={u.disabled ? undefined : `Disable ${u.name}? Its sessions end and its keys stop working until you enable it again.`}
              run={() => patch(`/users/${u.id}`, { disabled: !u.disabled }).then(r.reload)}
              done={u.disabled ? "Enabled." : "Disabled."}
            >
              {u.disabled ? "Enable" : "Disable"}
            </Action>
            <Action
              class="btn btn-danger"
              confirm={`Delete ${u.name} and all of its keys? This cannot be undone.`}
              run={async () => {
                await del(`/users/${u.id}`);
                toast(`Deleted ${u.name}.`);
                route("/access");
              }}
            >
              Delete
            </Action>
          </>
        )
      }
    >
      {!owner && <AccessTabs current="people" />}
      <div class="max-w-4xl space-y-6">
        {editable && projects.data && <EditAccess key={u.id} user={u} projects={projects.data.projects} onSaved={r.reload} />}
        {u.is_owner && <Note>The owner can do everything, so there is no access to set.</Note>}
        <Keys user={u} self={self} />
      </div>
    </Page>
  );
}

function EditAccess(props: { user: User; projects: Project[]; onSaved: () => void }) {
  const [access, setAccess] = useState(() => toAccess(props.user));
  const who = props.user.kind === "agent" ? "the agent" : "this person";
  const s = useSubmit(async () => {
    await patch(`/users/${props.user.id}`, accessBody(access));
    toast("Access saved.");
    props.onSaved();
  });
  return (
    <Card title="Access">
      <form class="space-y-4" onSubmit={s.submit}>
        <AccessEditor value={access} onChange={setAccess} projects={props.projects} who={who} />
        <ErrorBox error={s.error} />
        <button class="btn btn-primary" disabled={s.busy}>
          Save access
        </button>
      </form>
    </Card>
  );
}

function Keys(props: { user: User; self: boolean }) {
  const u = props.user;
  const keys = useApi<{ keys: Key[] }>(`/users/${u.id}/keys`);
  const [reveal, setReveal] = useState<{ token: string; snippets: Snippets } | null>(null);
  const [creating, setCreating] = useState(false);
  const { f, set, bind, reset } = useForm({ name: "", expires: "90", ips: "" });
  const s = useSubmit(async () => {
    const k = await post<{ id: string; token: string; snippets: Snippets }>(`/users/${u.id}/keys`, { name: f.name.trim() || undefined, ...keyBody(f) });
    setCreating(false);
    reset();
    setReveal(k);
    keys.reload();
  });
  const nowSec = Date.now() / 1000;
  return (
    <Card
      title="API keys"
      actions={
        <button class="btn" onClick={() => setCreating(true)}>
          New key
        </button>
      }
      pad={false}
    >
      <Loaded r={keys} skeleton={<RowsSkeleton />}>
        {(d) => (
          <Rows
            items={d.keys}
            empty={
              u.kind === "agent"
                ? "This agent has no keys, so it cannot reach the API until you create one."
                : "There are no API keys yet. Keys let scripts use the API as this user."
            }
          >
            {(k) => {
              const expired = k.expires_at != null && k.expires_at < nowSec;
              const state = k.revoked_at ? "revoked" : expired ? "expired" : "active";
              return (
                <li key={k.id} class={`row ${state === "active" ? "" : "opacity-60"}`}>
                  <div class="min-w-0 flex-1 space-y-1">
                    <div class="flex flex-wrap items-center gap-2">
                      <span class="font-medium">{k.name}</span>
                      <code class="text-xs text-muted">{k.prefix}…</code>
                      <span class={`chip ${state === "active" ? "text-ok" : "text-bad"}`}>{state}</span>
                    </div>
                    <p class="text-xs text-muted">
                      {k.last_used_at ? `Last used ${ago(k.last_used_at)}${k.last_used_ip ? ` from ${k.last_used_ip}` : ""}` : "Never used"}
                      {" · "}
                      {k.revoked_at ? `revoked ${ago(k.revoked_at)}` : k.expires_at ? `${expired ? "expired" : "expires"} ${when(k.expires_at)}` : "never expires"}
                      {" · "}
                      {k.allowed_ips.length ? `allowed from ${k.allowed_ips.join(", ")}` : "allowed from any address"}
                      {" · "}created {ago(k.created_at)}
                    </p>
                  </div>
                  {state !== "revoked" && (
                    <span class="flex gap-2">
                      <Action
                        confirm={`Rotate ${k.name}? The current token stops working right away.`}
                        run={async () => {
                          setReveal(await post(`/keys/${k.id}/rotate`));
                          keys.reload();
                        }}
                      >
                        Rotate
                      </Action>
                      <Action
                        class="btn btn-danger"
                        confirm={`Revoke ${k.name}? Anything using it loses access right away.`}
                        run={() => del(`/keys/${k.id}`).then(keys.reload)}
                        done="Key revoked."
                      >
                        Revoke
                      </Action>
                    </span>
                  )}
                </li>
              );
            }}
          </Rows>
        )}
      </Loaded>
      <Modal open={creating} onClose={() => setCreating(false)} title="New API key" wide>
        <form class="space-y-4" onSubmit={s.submit}>
          <Input label="Key name" hint="A name helps you tell keys apart, like the machine or pipeline that uses the key." autoFocus placeholder="laptop" {...bind("name")} />
          <KeyFields f={f} set={set} />
          <p class="text-xs text-muted">The key acts with {props.self ? "your" : `${u.name}'s`} access, no more and no less.</p>
          <FormButtons s={s} submit="Create key" onCancel={() => setCreating(false)} />
        </form>
      </Modal>
      <Modal open={!!reveal} onClose={() => setReveal(null)} title="New token" wide>
        {reveal && <TokenReveal token={reveal.token} snippets={reveal.snippets} />}
      </Modal>
    </Card>
  );
}

interface Entry {
  id: number;
  at: number;
  actor: string | null;
  actor_kind: string | null;
  key_id: string | null;
  ip: string | null;
  action: string;
  project_id: string | null;
  service_id: string | null;
  target: string | null;
  detail: string | null;
}
const PAGE = 100;

export function Audit() {
  const me = useMe();
  const projects = useProjects();
  const users = useApi<{ users: User[] }>(me.principal.is_owner ? "/users" : null);
  const [project, setProject] = useState("");
  const [actor, setActor] = useState("");
  const [entries, setEntries] = useState<Entry[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [more, setMore] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>();
  const names = Object.fromEntries((projects.data?.projects ?? []).map((p) => [p.id, p.name]));

  const load = async (before?: number) => {
    try {
      const r = await get<{ entries: Entry[] }>(`/audit${qs({ project, actor, before, limit: PAGE })}`);
      setEntries((cur) => (before ? [...(cur ?? []), ...r.entries] : r.entries));
      setMore(r.entries.length === PAGE);
      setError(null);
    } catch (e) {
      setError(e);
    }
  };
  useEffect(() => {
    setEntries(null);
    load();
  }, [project, actor]);
  // New entries arrive on the event stream; refresh the first page unless older pages are open.
  useEvent((e) => {
    if ((e.kind !== "audit" && e.kind !== "resync") || (entries?.length ?? 0) > PAGE) return;
    clearTimeout(timer.current);
    timer.current = setTimeout(() => load(), 500);
  });

  return (
    <Page title="Access">
      <AccessTabs current="audit" />
      <div class="mb-4 flex flex-wrap gap-3">
        <Field label="Project" class="min-w-44">
          <Select value={project} onChange={setProject} options={[["", "All projects"], ...(projects.data?.projects ?? []).map((p): [string, string] => [p.id, p.name])]} />
        </Field>
        <Field label="Who" class="min-w-44">
          {users.data ? (
            <Select
              value={actor}
              onChange={setActor}
              options={[["", "Everyone"], ...users.data.users.map((u): [string, string] => [u.name, `${u.name} (${u.kind})`]), ["tugport", "Tugport itself"]]}
            />
          ) : (
            <input class="input" placeholder="Name" value={actor} onChange={(e) => setActor(e.currentTarget.value.trim())} />
          )}
        </Field>
      </div>
      {error ? (
        <ErrorBox error={error} />
      ) : !entries ? (
        <Loading />
      ) : entries.length === 0 ? (
        <Empty>Nothing has been recorded for this filter yet.</Empty>
      ) : (
        <>
          <ul class="card divide-y divide-line">
            {entries.map((e) => (
              <li key={e.id} class="px-4 py-2.5 md:px-5">
                <div class="flex flex-wrap items-baseline gap-x-2 gap-y-1">
                  <span class="font-medium">{e.actor ?? "tugport"}</span>
                  {e.actor_kind && e.actor_kind !== "human" && <span class="chip">{e.actor_kind}</span>}
                  <code class="text-[13px] text-accent-text">{e.action}</code>
                  {e.target && <span class="min-w-0 break-all text-dim">{e.target}</span>}
                </div>
                <p class="mt-0.5 text-xs text-muted">
                  {[when(e.at), e.project_id ? (names[e.project_id] ?? e.project_id) : "", e.ip ?? "", e.key_id ? `key ${e.key_id}` : "", e.detail ?? ""]
                    .filter(Boolean)
                    .join(" · ")}
                </p>
              </li>
            ))}
          </ul>
          {more && (
            <div class="mt-4 text-center">
              <button class="btn" onClick={() => load(entries[entries.length - 1]?.id)}>
                Load older entries
              </button>
            </div>
          )}
        </>
      )}
    </Page>
  );
}
