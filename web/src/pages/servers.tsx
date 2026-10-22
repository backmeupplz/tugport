import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { route } from "preact-router";
import { canSeeServers, del, patch, post, useApi, useMe, type Event, type Node } from "../api";
import { Card, CodeBlock, Empty, ErrorBox, Field, Loaded, Modal, NameInput, Note, Page, Pill, toast, useSubmit } from "../ui";
import { ago, bytes, labelsText, parseLabels } from "../util";
import { DockerUsage, ServerCharts, TugportLog } from "./observe";

type Nodes = { mesh: boolean; nodes: Node[] };

// Joins and outages arrive as "nodes" events, and every 10-second sample refreshes "last seen".
// Neither reaches people other than the owner, so they poll.
const useServers = () => useApi<Nodes>(canSeeServers(useMe()) ? "/nodes" : null, (e: Event) => e.kind === "nodes" || e.kind === "metrics", 15000);

/** The status label and, when something is off, what it means. */
const STATUS: Record<string, [string, string]> = {
  online: ["online", ""],
  joining: ["joining", "It registered with the panel and is finishing its setup. It turns online as soon as its agent answers."],
  unreachable: [
    "not answering",
    "It missed its last health check, so Tugport sends it no traffic and no new containers for now. It counts as offline after 30 seconds without an answer.",
  ],
  offline: [
    "offline",
    "It has not answered for more than 30 seconds. Two minutes after it went away, services that can move start on the other servers, while services that keep data on it wait for it to come back.",
  ],
};
const status = (n: Node) => STATUS[n.status] ?? [n.status, ""];
const lastAnswer = (n: Node) => (n.last_seen_at ? ` It last answered ${ago(n.last_seen_at)}.` : "");

const NoAccess = () => (
  <Page title="Servers">
    <Note>Servers are visible to the owner and to people with a role on all projects.</Note>
  </Page>
);

export function Servers(_: { path?: string }) {
  const me = useMe();
  const r = useServers();
  const [adding, setAdding] = useState(false);
  const owner = me.principal.is_owner;
  if (!canSeeServers(me)) return <NoAccess />;
  return (
    <Page
      title="Servers"
      sub="These are the machines that run your services, with their health and capacity."
      actions={
        owner && (
          <button class="btn btn-primary" onClick={() => setAdding(true)}>
            Add a server
          </button>
        )
      }
    >
      <Loaded r={r}>
        {(d) => (
          <div class="space-y-6">
            <ul class="grid gap-4 lg:grid-cols-2">
              {d.nodes.map((n) => (
                <li key={n.id}>
                  <a href={`/servers/${n.id}`} class="card block h-full p-4 hover:border-accent/60">
                    <Heading node={n} />
                    {n.status !== "online" && (
                      <p class="mt-2 text-dim">
                        {status(n)[1]}
                        {lastAnswer(n)}
                      </p>
                    )}
                    <Facts node={n} class="mt-4" />
                  </a>
                </li>
              ))}
            </ul>
            {d.nodes.length === 1 && (
              <Card title="Add a second server">
                <div class="space-y-3 text-dim">
                  <p>
                    Everything runs on this server today. With a second one, Tugport spreads the replicas of each service over both servers, keeps serving from one when the
                    other goes down, and starts services on the remaining server when one stays away for two minutes.
                  </p>
                  <p>The servers talk over a private WireGuard network that Tugport sets up. A new server needs Linux and root access, and the install command sets up the rest.</p>
                  {owner ? (
                    <button class="btn btn-primary" onClick={() => setAdding(true)}>
                      Add a server
                    </button>
                  ) : (
                    <p class="text-xs text-muted">Only the owner can add servers.</p>
                  )}
                </div>
              </Card>
            )}
          </div>
        )}
      </Loaded>
      {adding && r.data && <AddServer list={r.data} onClose={() => setAdding(false)} />}
    </Page>
  );
}

function Heading({ node: n }: { node: Node }) {
  return (
    <div class="flex flex-wrap items-center gap-2">
      <h2 class="min-w-0 truncate text-base font-semibold">{n.name}</h2>
      {n.local && <span class="chip">runs the panel</span>}
      <Pill state={n.status} label={status(n)[0]} class="ml-auto" />
    </div>
  );
}

/** What a server reports about itself, plus its placement details. */
function Facts(props: { node: Node; class?: string }) {
  const n = props.node;
  const i = n.info ?? {};
  const labels = Object.entries(n.labels ?? {});
  const rows: [string, ComponentChildren][] = [
    ["Tugport", i.version ?? "Unknown"],
    ["Docker", i.docker ?? "Unknown"],
    ["Processor", i.cpus ? `${i.cpus} cores${i.arch ? `, ${i.arch}` : ""}` : "Unknown"],
    ["Memory", i.memory_total ? bytes(i.memory_total) : "Unknown"],
    ["GPU", i.nvidia ? "NVIDIA" : i.dri ? "Through /dev/dri" : i.nvidia === undefined ? "Unknown" : "None"],
    ["Region", n.region || "None"],
    ["Labels", labels.length ? labels.map(([k, v]) => <span key={k} class="chip mr-1 font-mono">{`${k}=${v}`}</span>) : "None"],
    ["Last seen", n.last_seen_at ? ago(n.last_seen_at) : "Never"],
  ];
  return (
    <dl class={`grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-4 ${props.class ?? ""}`}>
      {rows.map(([k, v]) => (
        <div key={k} class="min-w-0">
          <dt class="text-xs text-muted">{k}</dt>
          <dd class="break-words">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function ServerPage(props: { id?: string; path?: string }) {
  const me = useMe();
  const r = useServers();
  const [editing, setEditing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const id = props.id ?? "local";
  const owner = me.principal.is_owner;
  if (!canSeeServers(me)) return <NoAccess />;
  const n = r.data?.nodes.find((x) => x.id === id);
  if (!n) {
    return (
      <Page title="Server">
        <Loaded r={r}>{() => <Empty action={<a class="btn" href="/servers">Go to servers</a>}>This server is not part of the panel, or it was removed.</Empty>}</Loaded>
      </Page>
    );
  }
  const [label, why] = status(n);
  return (
    <Page
      title={n.name}
      sub={
        <span class="flex flex-wrap items-center gap-2">
          <span>
            <a class="link" href="/servers">
              Servers
            </a>{" "}
            / {n.name}
          </span>
          <Pill state={n.status} label={label} />
          {n.local && <span class="chip">runs the panel</span>}
        </span>
      }
      actions={
        owner && (
          <>
            <button class="btn" onClick={() => setEditing(true)}>
              Edit details
            </button>
            {!n.local && (
              <button class="btn btn-danger" onClick={() => setRemoving(true)}>
                Remove
              </button>
            )}
          </>
        )
      }
    >
      <div class="space-y-6">
        {why && (
          <Note tone={n.status === "offline" ? "bad" : "warn"}>
            {why}
            {lastAnswer(n)}
          </Note>
        )}
        <Card>
          <Facts node={n} />
          <p class="mt-4 text-xs text-muted">
            {n.mesh_ip
              ? `Its address on the private network between servers is ${n.mesh_ip}${n.local ? "." : `, and it joined ${ago(n.created_at)}.`}`
              : "It is the only server, so there is no private network between servers yet."}
          </p>
        </Card>
        <ServerCharts node={n.id} />
        {n.local && (
          <div class="grid gap-6 lg:grid-cols-2">
            <DockerUsage />
            {owner && <TugportLog />}
          </div>
        )}
      </div>
      {editing && <EditServer node={n} onClose={() => setEditing(false)} onSaved={r.reload} />}
      {removing && <RemoveServer node={n} onClose={() => setRemoving(false)} />}
    </Page>
  );
}

function AddServer(props: { list: Nodes; onClose: () => void }) {
  const [url, setUrl] = useState(location.origin);
  const [join, setJoin] = useState<{ command: string; expires_at: number; known: string[] } | null>(null);
  const s = useSubmit(async () => {
    const known = props.list.nodes.map((n) => n.id);
    const t = await post<{ command: string; expires_at: number }>("/nodes/join-token", { panel_url: url.trim().replace(/\/+$/, "") });
    setJoin({ ...t, known });
  });
  // The list behind the dialog refetches on "nodes" events, so new servers show up here too.
  const joined = join ? props.list.nodes.filter((n) => !join.known.includes(n.id)) : [];
  return (
    <Modal open onClose={props.onClose} title="Add a server" wide>
      {!join ? (
        <form class="space-y-4" onSubmit={s.submit}>
          <p class="text-dim">
            Tugport makes a one-line command for the new server. It installs Docker if the server lacks it, connects the server to your other servers over a private WireGuard
            network, and starts the Tugport agent there.
          </p>
          <Field
            label="Panel address"
            hint="The new server calls the panel at this address to join, so it must work from there. Use the panel domain or another address that the new server can reach."
          >
            <input class="input font-mono" type="url" required spellcheck={false} value={url} onInput={(e) => setUrl(e.currentTarget.value)} />
          </Field>
          {/^https?:\/\/(localhost|127\.|\[::1\])/i.test(url.trim()) && (
            <Note tone="warn">A new server cannot reach the panel at a localhost address, so enter one that works from that server.</Note>
          )}
          {!props.list.mesh && <Note>This is the first server you add, so Tugport turns on the private network on this server first. That takes a few seconds.</Note>}
          <ErrorBox error={s.error} />
          <div class="flex justify-end gap-2">
            <button type="button" class="btn" onClick={props.onClose}>
              Cancel
            </button>
            <button class="btn btn-primary" disabled={s.busy}>
              {s.busy ? (props.list.mesh ? "Creating the command…" : "Turning on the private network…") : "Create the command"}
            </button>
          </div>
        </form>
      ) : (
        <div class="space-y-4">
          <p class="text-dim">
            Run this command as root on the new server, for example after <code>sudo -i</code>.
          </p>
          <CodeBlock label="Install and join" text={join.command} />
          <ul class="list-disc space-y-1 pl-5 text-dim">
            <li>The command works once, and it expires in an hour, at {new Date(join.expires_at * 1000).toLocaleTimeString(undefined, { timeStyle: "short" })}.</li>
            <li>Servers talk to each other over UDP port 51820, so that port must be open between all of them.</li>
            <li>Every server that your domains' DNS records point at also needs TCP ports 80 and 443 open.</li>
            <li>The new server must reach the panel at {url.trim()}.</li>
          </ul>
          {joined.length > 0 ? (
            <Note tone="ok">
              {joined.map((n, i) => (
                <span key={n.id}>
                  {i > 0 && " "}
                  <a class="link font-medium" href={`/servers/${n.id}`} onClick={props.onClose}>
                    {n.name}
                  </a>{" "}
                  joined, and {n.status === "online" ? "it is online now." : n.status === "joining" ? "it is finishing its setup." : `it is ${status(n)[0]} right now.`}
                </span>
              ))}
            </Note>
          ) : (
            <p class="flex items-center gap-2 text-dim" role="status">
              <span class="size-2 shrink-0 rounded-full bg-warn motion-safe:animate-pulse" aria-hidden="true" />
              Waiting for the new server. It shows up here a few seconds after the command finishes.
            </p>
          )}
          <div class="flex flex-wrap justify-between gap-2">
            <button type="button" class="btn" onClick={() => setJoin(null)}>
              Change the panel address
            </button>
            <button type="button" class="btn btn-primary" onClick={props.onClose}>
              Done
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}

function EditServer(props: { node: Node; onClose: () => void; onSaved: () => void }) {
  const n = props.node;
  const [f, setF] = useState({ name: n.name, region: n.region ?? "", labels: labelsText(n.labels) });
  const upd = (p: Partial<typeof f>) => setF((s) => ({ ...s, ...p }));
  const s = useSubmit(async () => {
    const labels = parseLabels(f.labels);
    if (!labels) throw new Error("Write each label as key=value, separated by commas or new lines.");
    // The API ignores a null region, so an empty string clears it.
    await patch(`/nodes/${n.id}`, { ...(f.name !== n.name && { name: f.name }), region: f.region.trim(), labels });
    toast(`Saved ${f.name}.`);
    props.onSaved();
    props.onClose();
  });
  return (
    <Modal open onClose={props.onClose} title={`Edit ${n.name}`}>
      <form class="space-y-4" onSubmit={s.submit}>
        <Field label="Name" hint="Use lowercase letters, digits and dashes. Placements that name this server by its old name stop matching it.">
          <NameInput value={f.name} initial={n.name} onChange={(name) => upd({ name })} />
        </Field>
        <Field label="Region (optional)" hint="Services can ask for a region in their placement settings, like eu or us-east.">
          <input class="input font-mono" spellcheck={false} value={f.region} onInput={(e) => upd({ region: e.currentTarget.value })} />
        </Field>
        <Field label="Labels (optional)" hint="Write key=value pairs, separated by commas or new lines, like disk=ssd. Services can ask for servers with these labels.">
          <textarea class="input h-20" spellcheck={false} value={f.labels} onInput={(e) => upd({ labels: e.currentTarget.value })} />
        </Field>
        <p class="text-xs text-muted">Deploys read these details to pick servers, so running containers stay where they are until their service deploys again.</p>
        <ErrorBox error={s.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={s.busy}>
            Save
          </button>
        </div>
      </form>
    </Modal>
  );
}

function RemoveServer(props: { node: Node; onClose: () => void }) {
  const n = props.node;
  const [typed, setTyped] = useState("");
  const s = useSubmit(async () => {
    await del(`/nodes/${n.id}`);
    toast(`Removed ${n.name}.`);
    props.onClose();
    route("/servers");
  });
  return (
    <Modal open onClose={props.onClose} title={`Remove ${n.name}`}>
      <form class="space-y-4" onSubmit={s.submit}>
        <p class="text-dim">
          Removing {n.name} takes it off the private network and out of the swarm, and it stops the Tugport agent there. The server then no longer serves your domains, and
          Tugport starts no containers on it.
        </p>
        <ul class="list-disc space-y-1 pl-5 text-dim">
          <li>Containers on {n.name} are not deleted, but they stop getting traffic, and Tugport no longer manages them.</li>
          <li>Services with replicas on it run with fewer replicas until their next deploy places them on the remaining servers.</li>
          <li>
            Volumes and database files stay on {n.name}. A service that kept its data there starts with empty volumes on another server at its next deploy, so back up that
            data first.
          </li>
          <li>To bring the server back later, run a new join command on it.</li>
        </ul>
        {n.status !== "online" && (
          <Note tone="warn">
            {n.name} is not answering right now, so Tugport cannot tell it to leave. If it comes back, stop its agent there with <code>systemctl disable --now tugport-node</code>.
          </Note>
        )}
        <Field label={`Type ${n.name} to confirm`}>
          <input class="input font-mono" autoComplete="off" spellcheck={false} value={typed} onInput={(e) => setTyped(e.currentTarget.value)} />
        </Field>
        <ErrorBox error={s.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-danger" disabled={typed !== n.name || s.busy}>
            Remove {n.name}
          </button>
        </div>
      </form>
    </Modal>
  );
}
