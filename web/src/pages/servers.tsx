import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { canSeeServers, del, patch, post, useApi, useMe, type Event, type Node } from "../api";
import { currentUrl, route } from "../router";
import {
  Card,
  Check,
  CodeBlock,
  ConfirmName,
  Empty,
  ErrorBox,
  Field,
  FormButtons,
  Input,
  Loaded,
  Modal,
  NameInput,
  Note,
  Page,
  Pill,
  TextArea,
  toast,
  useForm,
  useSubmit,
} from "../ui";
import { ago, bytes, labelsText, requireLabels } from "../util";
import { PROVIDERS, ServerTabs, machineText, providerOf, useCloud, type Machine, type Pool } from "./cloud";
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
/** Broken swarm mode, as of the server's latest answer (an old answer may be stale). */
const swarmProblem = (n: Node) => (n.status === "online" ? n.info?.swarm_problem : null);
const lastAnswer = (n: Node) => (n.last_seen_at ? ` It last answered ${ago(n.last_seen_at)}.` : "");

const NoAccess = () => (
  <Page title="Servers">
    <Note>Servers are visible to the owner and to people with a role on all projects.</Note>
  </Page>
);

export function Servers() {
  const me = useMe();
  const r = useServers();
  const cloud = useCloud();
  const [adding, setAdding] = useState(false);
  const owner = me.principal.is_owner;
  if (!canSeeServers(me)) return <NoAccess />;
  const machines = cloud.machines.data?.servers ?? [];
  const creating = machines.filter((m) => m.status === "creating").length;
  const failed = machines.filter((m) => m.status === "failed").length;
  const provider = (n: Node) => {
    const m = machines.find((x) => x.node_id === n.id);
    const kind = m && cloud.accounts.data?.accounts.find((a) => a.id === m.account_id)?.kind;
    return m ? (kind ? (PROVIDERS[kind] ?? kind) : "a cloud provider") : "";
  };
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
      <ServerTabs current="list" />
      <Loaded r={r}>
        {(d) => (
          <div class="space-y-6">
            {(creating > 0 || failed > 0) && (
              <Note>
                {creating > 0 &&
                  `Tugport is setting up ${creating === 1 ? "a machine, which shows up here once it joins" : `${creating} machines, which show up here once they join`}. `}
                {failed > 0 && `${failed === 1 ? "A machine" : `${failed} machines`} could not be created. `}
                The{" "}
                <a class="link" href="/servers/cloud">
                  Cloud tab
                </a>{" "}
                has the details.
              </Note>
            )}
            <ul class="grid gap-4 lg:grid-cols-2">
              {d.nodes.map((n) => (
                <li key={n.id}>
                  <a href={`/servers/${n.id}`} class="card block h-full p-4 hover:border-accent/60">
                    <Heading node={n} provider={provider(n)} />
                    {n.status !== "online" && (
                      <p class="mt-2 text-dim">
                        {status(n)[1]}
                        {lastAnswer(n)}
                      </p>
                    )}
                    {swarmProblem(n) && <p class="mt-2 text-bad">{swarmProblem(n)}</p>}
                    <Facts node={n} class="mt-4" />
                  </a>
                </li>
              ))}
            </ul>
            {d.nodes.length === 1 && (
              <Card title="Add a second server">
                <div class="space-y-3 text-dim">
                  <p>
                    Everything runs on this server today. With a second one, Tugport spreads the replicas of each service over both servers, keeps serving from one when the other
                    goes down, and starts services on the remaining server when one stays away for two minutes.
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

function Heading({ node: n, provider }: { node: Node; provider: string }) {
  return (
    <div class="flex flex-wrap items-center gap-2">
      <h2 class="min-w-0 truncate text-base font-semibold">{n.name}</h2>
      {n.local && <span class="chip">runs the panel</span>}
      {provider && <span class="chip">created at {provider}</span>}
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
    ["Public traffic", n.ingress ? (n.local ? "Yes (the panel)" : "Yes, DNS may point here") : "No, only serves forwarded requests"],
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

export function ServerPage(props: { id?: string }) {
  const me = useMe();
  const r = useServers();
  const cloud = useCloud();
  const [editing, setEditing] = useState(false);
  const [removing, setRemoving] = useState(false);
  const id = props.id ?? "local";
  const owner = me.principal.is_owner;
  const accounts = cloud.accounts.data?.accounts;
  const machine = cloud.machines.data?.servers.find((m) => m.node_id === id);
  const pools = useApi<{ pools: Pool[] }>(machine?.pool_id ? "/cloud/pools" : null);
  const pool = pools.data?.pools.find((p) => p.id === machine?.pool_id);
  if (!canSeeServers(me)) return <NoAccess />;
  const n = r.data?.nodes.find((x) => x.id === id);
  if (!n) {
    return (
      <Page title="Server">
        <Loaded r={r}>
          {() => (
            <Empty
              action={
                <a class="btn" href="/servers">
                  Go to servers
                </a>
              }
            >
              This server is not part of the panel, or it was removed.
            </Empty>
          )}
        </Loaded>
      </Page>
    );
  }
  const [label, why] = status(n);
  return (
    <Page
      title={n.name}
      crumbs={[["Servers", "/servers"]]}
      meta={
        <>
          <Pill state={n.status} label={label} />
          {n.local && <span class="chip">runs the panel</span>}
        </>
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
        {swarmProblem(n) && <Note tone="bad">{swarmProblem(n)}</Note>}
        <Card>
          <Facts node={n} />
          <p class="mt-4 text-xs text-muted">
            {n.mesh_ip
              ? `Its address on the private network between servers is ${n.mesh_ip}${n.local ? "." : `, and it joined ${ago(n.created_at)}.`}`
              : "It is the only server, so there is no private network between servers yet."}
            {machine && ` Tugport created this machine, ${machineText(machine, accounts)}.`}
            {pool && ` It belongs to the pool ${pool.name}, which may remove it while the load stays low.`}
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
      {removing && <RemoveServer node={n} machine={machine} provider={providerOf(accounts, machine?.account_id)} onClose={() => setRemoving(false)} />}
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
          <p class="text-xs text-muted">
            Tugport can also create a server at Hetzner or AWS for you on the{" "}
            <a class="link" href="/servers/cloud" onClick={props.onClose}>
              Cloud tab
            </a>
            .
          </p>
          <Input
            label="Panel address"
            hint="The new server calls the panel at this address to join, so it must work from there. Use the panel domain or another address that the new server can reach."
            mono
            type="url"
            required
            value={url}
            onInput={(e) => setUrl(e.currentTarget.value)}
          />
          {/^https?:\/\/(localhost|127\.|\[::1\])/i.test(url.trim()) && (
            <Note tone="warn">A new server cannot reach the panel at a localhost address, so enter one that works from that server.</Note>
          )}
          {!props.list.mesh && <Note>This is the first server you add, so Tugport turns on the private network on this server first. That takes a few seconds.</Note>}
          <FormButtons s={s} submit={s.busy ? (props.list.mesh ? "Creating the command…" : "Turning on the private network…") : "Create the command"} onCancel={props.onClose} />
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
  const { f, set, bind } = useForm({ name: n.name, region: n.region ?? "", labels: labelsText(n.labels), ingress: n.ingress });
  const s = useSubmit(async () => {
    const labels = requireLabels(f.labels);
    // The API ignores a null region, so an empty string clears it.
    await patch(`/nodes/${n.id}`, {
      ...(f.name !== n.name && { name: f.name }),
      region: f.region.trim(),
      labels,
      ...(!n.local && f.ingress !== n.ingress && { ingress: f.ingress }),
    });
    toast(`Saved ${f.name}.`);
    props.onSaved();
    props.onClose();
  });
  return (
    <Modal open onClose={props.onClose} title={`Edit ${n.name}`}>
      <form class="space-y-4" onSubmit={s.submit}>
        <Field label="Name" hint="Use lowercase letters, digits and dashes. Placements that name this server by its old name stop matching it.">
          <NameInput value={f.name} initial={n.name} onChange={(name) => set({ name })} />
        </Field>
        <Input label="Region (optional)" hint="Services can ask for a region in their placement settings, like eu or us-east." mono {...bind("region")} />
        <TextArea
          label="Labels (optional)"
          hint="Write key=value pairs, separated by commas or new lines, like disk=ssd. Services can ask for servers with these labels."
          class="h-20"
          {...bind("labels")}
        />
        {!n.local && (
          <Check
            checked={f.ingress}
            onChange={(ingress) => set({ ingress })}
            label="Let public DNS point at this server"
            hint="Turn this on only if a domain's DNS points directly at this server. It then receives the TLS certificates and the sign-in settings for every site. Leave it off and this server only serves requests forwarded from the panel, holding no certificates or passwords."
          />
        )}
        <p class="text-xs text-muted">Deploys read these details to pick servers, so running containers stay where they are until their service deploys again.</p>
        <FormButtons s={s} submit="Save" onCancel={props.onClose} />
      </form>
    </Modal>
  );
}

function RemoveServer(props: { node: Node; machine?: Machine; provider: string; onClose: () => void }) {
  const n = props.node;
  const m = props.machine;
  const provider = props.provider;
  const online = n.status === "online";
  const [typed, setTyped] = useState("");
  const [keep, setKeep] = useState(false);
  const deletes = !!m && !keep;
  const s = useSubmit(async () => {
    const r = await del<{ machine_deleted?: boolean }>(`/nodes/${n.id}${keep ? "?keep_machine=true" : ""}`);
    toast(r.machine_deleted ? `Removed ${n.name} and deleted its machine at ${provider}.` : `Removed ${n.name}.`);
    props.onClose();
    // The removal outlives the dialog, so leave the page only while it still shows this server.
    if (currentUrl() === `/servers/${n.id}`) route("/servers");
  });
  return (
    <Modal open onClose={props.onClose} title={`Remove ${n.name}`}>
      <form class="space-y-4" onSubmit={s.submit}>
        <p class="text-dim">
          Removing {n.name} takes it off the private network and out of the swarm, and it stops the Tugport agent there, so the server no longer serves your domains or runs new
          containers.
        </p>
        <ul class="list-disc space-y-1 pl-5 text-dim">
          {online ? (
            <li>First, Tugport moves every service that can move to the other servers, which can take a few minutes. The server gets no new containers meanwhile.</li>
          ) : (
            <li>
              Because {n.name} is not answering, Tugport cannot move its services first. Services that can move start on other servers by themselves once it has been away for two
              minutes.
            </li>
          )}
          <li>
            Services that keep data on {n.name}, such as databases and services with volumes, do not move. Their data stays there, and their next deploy starts them on another
            server with empty volumes, so back the data up first.
          </li>
          {!m ? (
            <li>
              The machine itself keeps running. Containers left on it get no traffic, and Tugport no longer manages them. To bring it back later, run a new join command on it.
            </li>
          ) : deletes ? (
            <li>Then Tugport deletes the machine at {provider}, together with its disk and everything on it.</li>
          ) : (
            <li>The machine keeps running at {provider} and costs money until you delete it there. Containers left on it get no traffic, and Tugport no longer manages them.</li>
          )}
        </ul>
        {m && (
          <Check
            checked={keep}
            onChange={setKeep}
            label={`Keep the machine at ${provider}`}
            hint="Tugport created this machine, so removing the server deletes the machine too unless you keep it."
          />
        )}
        {!online && !deletes && (
          <Note tone="warn">
            Tugport cannot tell {n.name} to leave while it is not answering. If it comes back, stop its agent there with <code>systemctl disable --now tugport-node</code>.
          </Note>
        )}
        <ConfirmName name={n.name} value={typed} onChange={setTyped} />
        <ErrorBox error={s.error} />
        {s.busy && <p class="text-xs text-muted">The removal goes on even if you close this dialog.</p>}
        <FormButtons
          s={{ busy: s.busy }}
          danger
          disabled={typed !== n.name}
          cancel={s.busy ? "Close" : "Cancel"}
          onCancel={props.onClose}
          submit={s.busy ? (online ? "Moving services and removing…" : "Removing…") : deletes ? `Remove ${n.name} and delete the machine` : `Remove ${n.name}`}
        />
      </form>
    </Modal>
  );
}
