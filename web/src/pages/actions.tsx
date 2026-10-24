import { useState } from "preact/hooks";
import { useApi, type Deploy } from "../api";
import { Icon } from "../icons";
import { Card, Loaded, Page, Pill, Rows, RowsSkeleton } from "../ui";
import { ago, duration } from "../util";
import { DeployLog } from "./service";

type DeployItem = Deploy & { service: string; project: string };
interface RunItem {
  id: string;
  job_id: string;
  kind: string;
  status: string;
  size_bytes: number | null;
  error: string | null;
  started_at: number;
  finished_at: number | null;
  job_kind: string;
  volume: string | null;
  service: string | null;
  project: string | null;
}
type Item = { at: number; deploy?: DeployItem; run?: RunItem };

const FILTERS: [string, string][] = [
  ["all", "Everything"],
  ["deploys", "Deploys"],
  ["backups", "Backups"],
  ["failed", "Failed"],
];

/** A backup run's status, in the words and colors the rest of the panel uses. */
const runState = (s: string): [string, string] => (s === "ok" ? ["live", "done"] : s === "running" ? ["building", "running"] : [s, s]);

const took = (start: number | null, end: number | null) => (start && end ? ` · took ${duration(end - start)}` : "");

/** Deploys and backups across everything the viewer can see, newest first. */
export function Actions() {
  const deploys = useApi<{ deploys: DeployItem[] }>("/deploys", (e) => e.kind === "deploy", 15000);
  // Backup jobs are on schedules and send no events, so the list polls.
  const runs = useApi<{ runs: RunItem[] }>("/backups/runs", undefined, 15000);
  const [filter, setFilter] = useState("all");
  const [logId, setLogId] = useState<string | null>(null);
  const items: Item[] = [
    ...(filter === "backups" ? [] : (deploys.data?.deploys ?? []).map((d): Item => ({ at: d.created_at, deploy: d }))),
    ...(filter === "deploys" ? [] : (runs.data?.runs ?? []).map((r): Item => ({ at: r.started_at, run: r }))),
  ]
    .filter((x) => filter !== "failed" || x.deploy?.status === "failed" || x.run?.status === "failed")
    .sort((a, b) => b.at - a.at);
  return (
    <Page title="Actions" sub="Deploys and backups across your projects, newest first.">
      <div role="group" aria-label="Show" class="mb-4 flex flex-wrap gap-1.5">
        {FILTERS.map(([id, label]) => (
          <button key={id} type="button" aria-pressed={filter === id} class={`btn btn-sm ${filter === id ? "border-accent/60 text-text" : "text-dim"}`} onClick={() => setFilter(id)}>
            {label}
          </button>
        ))}
      </div>
      <Card pad={false}>
        <Loaded r={deploys} skeleton={<RowsSkeleton n={6} />}>
          {() => (
            <Rows items={items} empty={filter === "failed" ? "Nothing failed recently." : "Nothing has run yet."}>
              {(x) => (x.deploy ? <DeployRow key={x.deploy.id} d={x.deploy} open={() => setLogId(x.deploy!.id)} /> : <RunRow key={x.run!.id} r={x.run!} />)}
            </Rows>
          )}
        </Loaded>
      </Card>
      <DeployLog id={logId} onClose={() => setLogId(null)} />
    </Page>
  );
}

function DeployRow(props: { d: DeployItem; open: () => void }) {
  const d = props.d;
  const what = d.commit_message ? d.commit_message.split("\n")[0] : d.trigger;
  return (
    <li>
      <button type="button" class="row w-full flex-nowrap text-left hover:bg-raised/60" onClick={props.open}>
        <span class="grid size-8 shrink-0 place-items-center rounded-lg border border-line bg-raised text-dim" title="Deploy">
          <Icon name="app" label="Deploy" />
        </span>
        <span class="min-w-0 flex-1">
          <span class="block truncate font-medium">
            {d.project}/{d.service}
          </span>
          <span class="block truncate text-xs text-muted">
            {what}
            {d.actor ? ` · by ${d.actor}` : ""} · {ago(d.created_at)}
            {took(d.started_at, d.finished_at)}
          </span>
          {d.status === "failed" && d.error && <span class="mt-0.5 block truncate text-xs text-bad">{d.error}</span>}
        </span>
        <Pill state={d.status} />
      </button>
    </li>
  );
}

function RunRow(props: { r: RunItem }) {
  const r = props.r;
  const [state, label] = runState(r.status);
  const what = r.job_kind === "state" ? "Tugport's own data" : r.job_kind === "volume" ? `volume ${r.volume ?? ""}` : "database";
  return (
    <li>
      <a href="/settings/backups" class="row flex-nowrap hover:bg-raised/60">
        <span class="grid size-8 shrink-0 place-items-center rounded-lg border border-line bg-raised text-dim" title="Backup">
          <Icon name="database" label="Backup" />
        </span>
        <span class="min-w-0 flex-1">
          <span class="block truncate font-medium">{r.service ? `${r.project}/${r.service}` : "Tugport"}</span>
          <span class="block truncate text-xs text-muted">
            {r.kind === "backup" ? "Backup" : r.kind === "test-restore" ? "Test restore" : "Restore"} of the {what} · {ago(r.started_at)}
            {took(r.started_at, r.finished_at)}
          </span>
          {r.status === "failed" && r.error && <span class="mt-0.5 block truncate text-xs text-bad">{r.error}</span>}
        </span>
        <Pill state={state} label={label} />
      </a>
    </li>
  );
}
