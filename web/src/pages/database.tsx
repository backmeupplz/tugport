// Database read replicas: the list and the add form on a primary, and promotion on a replica.
import { useState } from "preact/hooks";
import { atLeast, nodeName, post, useApi, type Event, type ServiceDetail } from "../api";
import { Card, Empty, ErrorBox, Field, Loaded, Modal, Note, Pill, toast, useSubmit } from "../ui";
import { parseLabels, varName } from "../util";
import type { Ctx } from "./service";
import { PlacementFields } from "./service-settings";

interface ReadReplica {
  id: string;
  name: string;
  state: string;
  nodes: string[];
  /** "copying", "following" or "failed: <reason>". */
  replication: string;
}

const ENGINES: Record<string, string> = { postgres: "PostgreSQL", mysql: "MySQL", mariadb: "MariaDB", mongo: "MongoDB", redis: "Redis" };

/** Replication as a pill, with the reason when it failed. */
function Replication(props: { state: string }) {
  const failed = props.state.startsWith("failed");
  const label = props.state === "copying" ? "copying the data" : failed ? "not replicating" : props.state;
  return (
    <>
      <Pill state={failed ? "failed" : props.state} label={label} />
      {failed && <span class="w-full text-sm text-bad">{props.state.replace(/^failed:\s*/, "")}</span>}
    </>
  );
}

/** The "Read replicas" tab of a primary database. */
export function ReadReplicas({ c }: { c: Ctx }) {
  const s = c.d.service;
  // A replica reports "following" on its own service, so any status change in the project may matter.
  const r = useApi<{ replicas: ReadReplica[] }>(`/services/${s.id}/replicas`, (e: Event) => e.project_id === s.project_id && e.kind !== "deploy.log");
  const [adding, setAdding] = useState(false);
  const mongo = s.spec.database?.engine === "mongo";
  return (
    <Card
      title="Read replicas"
      actions={
        atLeast(c.d.role, "operator") && (
          <button class="btn btn-primary" onClick={() => setAdding(true)}>
            Add a replica
          </button>
        )
      }
    >
      <div class="space-y-4">
        <p class="text-dim">
          A read replica is a copy of {s.name} that follows every change. It answers reads, which takes load off {s.name}, and it can run on another server or in another region.
          If {s.name} is lost, you can promote a replica to take its place.
        </p>
        <Loaded r={r}>
          {(d) =>
            d.replicas.length === 0 ? (
              <Empty>{s.name} has no read replicas yet.</Empty>
            ) : (
              <>
                <ul class="divide-y divide-line rounded-md border border-line">
                  {d.replicas.map((x) => (
                    <li key={x.id} class="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                      <a class="link min-w-0 flex-1 truncate font-medium" href={`/projects/${s.project}/${x.name}`}>
                        {x.name}
                      </a>
                      {[...new Set(x.nodes)].map((n) => (
                        <span key={n} class="chip">
                          on {nodeName(c.nodes, n)}
                        </span>
                      ))}
                      <Pill state={x.state} />
                      <Replication state={x.replication} />
                    </li>
                  ))}
                </ul>
                <p class="text-xs text-muted">
                  Services reach the replicas through the project variable <code>{varName(s.name, "READ_URL")}</code>, which points at{" "}
                  {mongo ? "the whole replica set and prefers replicas for reads" : "the first replica"}, and each replica has its own, like{" "}
                  <code>{varName(d.replicas[0].name, "URL")}</code>. Writes keep going to <code>{varName(s.name, "URL")}</code>. Services pick up changed variables on their next
                  deploy or restart.
                </p>
              </>
            )
          }
        </Loaded>
      </div>
      {adding && <AddReplica c={c} onClose={() => setAdding(false)} onAdded={r.reload} />}
    </Card>
  );
}

function AddReplica(props: { c: Ctx; onClose: () => void; onAdded: () => void }) {
  const { c } = props;
  const s = c.d.service;
  const nodes = c.nodes?.nodes;
  // A replica with no rules lands on the panel's server, so suggest a server the primary does not run on.
  const here = new Set(c.d.status.replicas.map((x) => x.node));
  const other = nodes?.find((n) => !here.has(n.id) && (n.local || n.status === "online"));
  const [rules, setRules] = useState({ node: other?.id ?? "", region: "", labels: "" });
  const engine = s.spec.database?.engine ?? "";
  // MongoDB becomes a replica set and MariaDB turns on binary logging, each with one restart.
  const restarts =
    (engine === "mongo" && !(s.spec.entrypoint ?? []).join(" ").includes("--replSet")) ||
    (engine === "mariadb" && !(s.spec.command ?? []).some((a: string) => a.startsWith("--log-bin")));
  const sub = useSubmit(async () => {
    const labels = parseLabels(rules.labels);
    if (!labels) throw new Error("Write each label as key=value, separated by commas or new lines.");
    const placement = { node: rules.node.trim() || null, region: rules.region.trim() || null, labels };
    const res = await post<{ replica: { id: string; name: string }; deploy_id: string }>(`/services/${s.id}/replicas`, { placement });
    toast(`Added ${res.replica.name}. It copies the data first and then follows ${s.name}.`);
    props.onAdded();
    props.onClose();
    c.openLog(res.deploy_id);
  });
  return (
    <Modal open onClose={props.onClose} title={`Add a read replica of ${s.name}`} wide>
      <form class="space-y-4" onSubmit={sub.submit}>
        <p class="text-dim">
          The replica is a database service of its own, named {s.name}-replica-N, in this project. It first copies all of {s.name}'s data, which takes a while for a large
          database, and then follows every change.
        </p>
        <PlacementFields nodes={nodes} rules={rules} set={(p) => setRules((x) => ({ ...x, ...p }))} />
        <p class="text-xs text-muted">
          With no rules, the replica runs on the panel's server. A replica on another server or in another region keeps a copy of the data when the primary's server goes down.
        </p>
        {restarts && (
          <Note tone="warn">
            {ENGINES[engine]} needs replication turned on first, so adding the first replica restarts {s.name} once. Connections to it drop for a few seconds.
          </Note>
        )}
        <ErrorBox error={sub.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={sub.busy}>
            {sub.busy ? (restarts ? `Restarting ${s.name} and adding the replica…` : "Adding the replica…") : "Add the replica"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** On a replica's overview: what it follows, how replication goes, and promotion. */
export function ReplicaOf({ c, primaryId }: { c: Ctx; primaryId: string }) {
  const s = c.d.service;
  const primary = useApi<ServiceDetail>(`/services/${primaryId}`);
  const siblings = useApi<{ replicas: ReadReplica[] }>(`/services/${primaryId}/replicas`, (e: Event) => e.service_id === s.id && e.kind === "service.status");
  const [promoting, setPromoting] = useState(false);
  const name = primary.data?.service.name;
  const mine = siblings.data?.replicas.find((x) => x.id === s.id);
  return (
    <Card
      title="Read replica"
      actions={
        atLeast(c.d.role, "admin") && (
          <button class="btn btn-danger" onClick={() => setPromoting(true)} disabled={!name}>
            Promote
          </button>
        )
      }
    >
      <div class="space-y-3">
        <p class="text-dim">
          This database follows{" "}
          {name ? (
            <a class="link" href={`/projects/${s.project}/${name}`}>
              {name}
            </a>
          ) : (
            "its primary"
          )}{" "}
          and copies every change from it. It answers reads only.
        </p>
        {mine && (
          <p class="flex flex-wrap items-center gap-2">
            <Replication state={mine.replication} />
          </p>
        )}
      </div>
      {promoting && name && <Promote c={c} primary={name} onClose={() => setPromoting(false)} />}
    </Card>
  );
}

function Promote(props: { c: Ctx; primary: string; onClose: () => void }) {
  const s = props.c.d.service;
  const [typed, setTyped] = useState("");
  const [message, setMessage] = useState("");
  const sub = useSubmit(async () => {
    setMessage((await post<{ variable: string; message: string }>(`/services/${s.id}/promote`)).message);
  });
  // The page shows a primary once the dialog closes, so the outcome stays here until then.
  const close = () => {
    props.onClose();
    if (message) props.c.reload();
  };
  return (
    <Modal open onClose={close} title={`Promote ${s.name}`}>
      {message ? (
        <div class="space-y-4">
          <Note tone="ok">{message}</Note>
          <div class="flex justify-end">
            <button type="button" class="btn btn-primary" onClick={close}>
              Done
            </button>
          </div>
        </div>
      ) : (
        <form class="space-y-4" onSubmit={sub.submit}>
          <p class="text-dim">
            Promote a replica when {props.primary} is lost. {s.name} then stops following it and accepts writes, and the project variable{" "}
            <code>{varName(props.primary, "URL")}</code> points at {s.name}, so services use it from their next deploy or restart.
          </p>
          <Note tone="bad">
            If {props.primary} is still running, both databases accept writes afterwards and their data drifts apart for good. Stop {props.primary} before you promote, and keep it
            from coming back.
          </Note>
          <Field label={`Type ${s.name} to confirm`}>
            <input class="input font-mono" autoComplete="off" spellcheck={false} value={typed} onInput={(e) => setTyped(e.currentTarget.value)} />
          </Field>
          <ErrorBox error={sub.error} />
          <div class="flex justify-end gap-2">
            <button type="button" class="btn" onClick={close}>
              Cancel
            </button>
            <button class="btn btn-danger" disabled={typed !== s.name || sub.busy}>
              {sub.busy ? "Promoting…" : `Promote ${s.name}`}
            </button>
          </div>
        </form>
      )}
    </Modal>
  );
}
