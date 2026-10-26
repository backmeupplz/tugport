import { useEffect, useState } from "preact/hooks";
import { get, useApi, useEvent, type Deploy } from "../api";
import { Icon } from "../icons";
import { Card, ErrorBox, Loaded, Modal, Page, Pill, Rows, RowsSkeleton } from "../ui";
import { ago, bytes, clock, duration, stripAnsi } from "../util";
import { tailLines, useStickToBottom } from "./observe";
import { DeployLog } from "./service";

type DeployItem = Deploy & { service: string; project: string };
/** How far a running backup or restore is, as restic last reported it. */
export interface RunProgress {
  percent: number;
  bytes_done: number;
  total_bytes: number;
  files_done: number;
  total_files: number;
  seconds_remaining?: number | null;
}
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
  progress?: RunProgress | null;
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

const runKind = (kind: string) => (kind === "backup" ? "Backup" : kind === "test-restore" ? "Test restore" : "Restore");
const runWhat = (r: RunItem) => (r.job_kind === "state" ? "Tugport's own data" : r.job_kind === "volume" ? `${r.volume?.startsWith("/") ? "folder" : "volume"} ${r.volume ?? ""}` : "database");

/** Deploys and backups across everything the viewer can see, newest first. */
export function Actions() {
  const deploys = useApi<{ deploys: DeployItem[] }>("/deploys", (e) => e.kind === "deploy", 15000);
  // Runs that start or end send "backup" events, but only to those who may see them; the list also polls.
  const runs = useApi<{ runs: RunItem[] }>("/backups/runs", (e) => e.kind === "backup", 15000);
  const [filter, setFilter] = useState("all");
  const [logId, setLogId] = useState<string | null>(null);
  const [runLogId, setRunLogId] = useState<string | null>(null);
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
              {(x) =>
                x.deploy ? <DeployRow key={x.deploy.id} d={x.deploy} open={() => setLogId(x.deploy!.id)} /> : <RunRow key={x.run!.id} r={x.run!} open={() => setRunLogId(x.run!.id)} />
              }
            </Rows>
          )}
        </Loaded>
      </Card>
      <DeployLog id={logId} onClose={() => setLogId(null)} />
      <RunLog id={runLogId} onClose={() => setRunLogId(null)} />
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

function RunRow(props: { r: RunItem; open: () => void }) {
  const r = props.r;
  const [state, label] = runState(r.status);
  return (
    <li>
      <button type="button" class="row w-full flex-nowrap text-left hover:bg-raised/60" onClick={props.open}>
        <span class="grid size-8 shrink-0 place-items-center rounded-lg border border-line bg-raised text-dim" title="Backup">
          <Icon name="database" label="Backup" />
        </span>
        <span class="min-w-0 flex-1">
          <span class="block truncate font-medium">{r.service ? `${r.project}/${r.service}` : "Tugport"}</span>
          <span class="block truncate text-xs text-muted">
            {runKind(r.kind)} of the {runWhat(r)} · {ago(r.started_at)}
            {took(r.started_at, r.finished_at)}
          </span>
          {r.status === "running" && <Progress id={r.id} initial={r.progress} class="mt-1" />}
          {r.status === "failed" && r.error && <span class="mt-0.5 block truncate text-xs text-bad">{r.error}</span>}
        </span>
        <Pill state={state} label={label} />
      </button>
    </li>
  );
}

/** A running run's progress, kept current from the event stream. */
export function Progress(props: { id: string; initial?: RunProgress | null; class?: string }) {
  const [p, setP] = useState(props.initial ?? null);
  useEffect(() => setP(props.initial ?? null), [props.id, props.initial]);
  useEvent((e) => {
    if (e.kind === "backup.progress" && e.data.run_id === props.id) setP(e.data.progress);
  });
  return <ProgressBar p={p} class={props.class} />;
}

function ProgressBar(props: { p: RunProgress | null; class?: string }) {
  const p = props.p;
  const pct = p && p.total_bytes > 0 ? Math.min(100, Math.max(0, p.percent)) : null;
  const text = !p
    ? "Starting…"
    : pct == null
      ? `${bytes(p.bytes_done)} so far`
      : `${bytes(p.bytes_done)} of ${bytes(p.total_bytes)} · ${Math.floor(pct)}%${p.seconds_remaining ? ` · about ${duration(p.seconds_remaining)} left` : ""}`;
  return (
    <span class={`block min-w-0 ${props.class ?? ""}`}>
      {pct != null && (
        <span class="mb-1 block h-1.5 max-w-xs overflow-hidden rounded-full bg-raised" role="progressbar" aria-valuenow={Math.round(pct)} aria-valuemin={0} aria-valuemax={100} aria-label="Progress">
          <span class="block h-full rounded-full bg-accent" style={{ width: `${pct}%` }} />
        </span>
      )}
      <span class="block truncate text-xs text-dim">{text}</span>
    </span>
  );
}

/** A backup or restore run's log, streamed live with its progress until the run finishes. */
export function RunLog(props: { id: string | null; onClose: () => void }) {
  const [lines, setLines] = useState<{ ts: number; line: string }[]>([]);
  const [status, setStatus] = useState("");
  const [progress, setProgress] = useState<RunProgress | null>(null);
  const [info, setInfo] = useState<RunItem | null>(null);
  const box = useStickToBottom<HTMLPreElement>(lines);
  const id = props.id;

  useEffect(() => {
    if (!id) return;
    setLines([]);
    setStatus("");
    setProgress(null);
    setInfo(null);
    box.stick.current = true;
    const refresh = () =>
      get<{ run: RunItem }>(`/backups/runs/${id}`)
        .then((r) => {
          setInfo(r.run);
          setStatus((s) => s || r.run.status);
        })
        .catch(() => {});
    refresh();
    const es = new EventSource(`/api/backups/runs/${id}/log`);
    const tail = tailLines<{ ts: number; line: string }>(es, (chunk) => setLines((l) => l.concat(chunk)));
    // A reconnect replays the whole log, so start over when the stream (re)opens.
    es.onopen = () => {
      tail.clear();
      setLines([]);
    };
    es.addEventListener("progress", (m) => {
      try {
        setProgress(JSON.parse(m.data));
      } catch {
        // A malformed event changes nothing.
      }
    });
    es.addEventListener("status", (m) => {
      setStatus(m.data);
      // The server ends the stream after the final status; closing stops the browser from reconnecting.
      if (m.data !== "running") {
        es.close();
        refresh();
      }
    });
    return tail.stop;
  }, [id]);

  const [state, label] = runState(status || "running");
  const done = !!status && status !== "running";
  return (
    <Modal open={!!id} onClose={props.onClose} title={info ? `${runKind(info.kind)} log` : "Backup log"} wide>
      <div class="mb-3 flex flex-wrap items-center gap-2">
        <Pill state={state} label={label} />
        {info && (
          <span class="text-dim">
            {info.service ? `${info.project}/${info.service}` : "Tugport"}, the {runWhat(info)} · {ago(info.started_at)}
            {took(info.started_at, info.finished_at)}
          </span>
        )}
      </div>
      {!done && <ProgressBar p={progress} class="mb-3" />}
      <pre
        ref={box.ref}
        tabIndex={0}
        aria-label="Run log"
        onScroll={box.onScroll}
        class="h-[60vh] overflow-auto whitespace-pre-wrap break-words rounded-md border border-line bg-bg p-3 font-mono text-xs leading-relaxed"
      >
        {lines.length ? lines.map((l) => `${clock(l.ts)}  ${stripAnsi(l.line)}`).join("\n") : done ? "This run kept no log. Runs from before Tugport recorded them have none." : "Waiting for the first lines…"}
      </pre>
      {info?.error && done && (
        <div class="mt-3">
          <ErrorBox error={info.error} />
        </div>
      )}
    </Modal>
  );
}
