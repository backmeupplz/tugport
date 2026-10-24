// Logs, charts and the console of services, and the charts of servers.
import type { JSX } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { get, useApi, type Node, type Replica, type Service } from "../api";
import { Chart, RANGES, fmt, type Points } from "../charts";
import { Card, Empty, ErrorBox, Field, Input, Loaded, Loading, Note, Pill, Select, Skel, errText } from "../ui";
import { ago, bytes, clock, qs, sentence, toMs } from "../util";

/** A log box that stays scrolled to the newest line, unless the reader scrolled up. `lines` changes as lines arrive. */
export function useStickToBottom<T extends HTMLElement>(lines: unknown) {
  const ref = useRef<T>(null);
  const stick = useRef(true);
  useLayoutEffect(() => {
    const b = ref.current;
    if (b && stick.current) b.scrollTop = b.scrollHeight;
  }, [lines]);
  const onScroll = (e: JSX.TargetedEvent<T>) => {
    const b = e.currentTarget;
    stick.current = b.scrollHeight - b.scrollTop - b.clientHeight < 40;
  };
  return { ref, stick, onScroll };
}

/** Parses the "line" events of a log stream and hands the kept ones over once per frame, so a burst renders once. */
export function tailLines<T>(es: EventSource, flush: (chunk: T[]) => void, keep: (l: T) => boolean = () => true) {
  let buf: T[] = [];
  let frame = 0;
  es.addEventListener("line", (m) => {
    let l: T;
    try {
      l = JSON.parse(m.data);
    } catch {
      return;
    }
    if (!keep(l)) return;
    buf.push(l);
    if (!frame)
      frame = requestAnimationFrame(() => {
        frame = 0;
        const chunk = buf;
        buf = [];
        flush(chunk);
      });
  });
  return {
    clear: () => {
      buf = [];
    },
    stop: () => {
      es.close();
      cancelAnimationFrame(frame);
    },
  };
}

interface LogLine {
  ts: number;
  replica?: string;
  stream?: string;
  line: string;
}
const LOG_RANGES: [string, string, number][] = [
  ["15m", "Last 15 minutes", 15 * 60e3],
  ["1h", "Last hour", 3600e3],
  ["6h", "Last 6 hours", 6 * 3600e3],
  ["24h", "Last 24 hours", 86400e3],
  ["7d", "Last 7 days", 7 * 86400e3],
];
const MAX_LINES = 5000;

export function Logs(props: { service: Service; replicas: Replica[]; label: (r: Replica) => string }) {
  const id = props.service.id;
  const [q, setQ] = useState("");
  const [query, setQuery] = useState("");
  const [range, setRange] = useState("1h");
  const [replica, setReplica] = useState("");
  const [live, setLive] = useState(true);
  const [lines, setLines] = useState<LogLine[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const box = useStickToBottom<HTMLDivElement>(lines);

  useEffect(() => {
    const t = setTimeout(() => setQuery(q.trim()), 300);
    return () => clearTimeout(t);
  }, [q]);

  useEffect(() => {
    let stale = false;
    setLines(null);
    setError(null);
    const since = Date.now() - (LOG_RANGES.find((r) => r[0] === range)?.[2] ?? 3600e3);
    get<{ lines: LogLine[] }>(`/services/${id}/logs${qs({ since, q: query, limit: 1000, replica })}`).then(
      (r) => !stale && setLines(r.lines),
      (e) => !stale && setError(e),
    );
    return () => {
      stale = true;
    };
  }, [id, query, range, replica]);

  const ready = lines !== null;
  useEffect(() => {
    if (!live || !ready) return;
    const needle = query.toLowerCase();
    return tailLines<LogLine>(
      new EventSource(`/api/services/${id}/logs/stream`),
      (chunk) => setLines((cur) => (cur ?? []).concat(chunk).slice(-MAX_LINES)),
      (l) => (!replica || l.replica === replica) && (!needle || l.line.toLowerCase().includes(needle)),
    ).stop;
  }, [id, live, ready, query, replica]);

  const many = new Set(props.replicas.map((r) => r.name)).size > 1;
  return (
    <div class="space-y-4">
      <div class="flex flex-wrap items-end gap-3">
        <Input label="Search" fieldClass="min-w-48 flex-1" type="search" placeholder="Text to find, any case" value={q} onInput={(e) => setQ(e.currentTarget.value)} />
        <Field label="Time range">
          <Select value={range} onChange={setRange} options={LOG_RANGES.map(([v, l]): [string, string] => [v, l])} />
        </Field>
        {props.replicas.length > 1 && (
          <Field label="Replica">
            <Select value={replica} onChange={setReplica} options={[["", "All replicas"], ...props.replicas.map((r): [string, string] => [r.name, props.label(r)])]} />
          </Field>
        )}
        <label class="flex items-center gap-2 pb-2">
          <input type="checkbox" checked={live} onChange={(e) => setLive(e.currentTarget.checked)} />
          Follow new lines
        </label>
      </div>
      {error ? (
        <ErrorBox error={error} />
      ) : !lines ? (
        <Loading />
      ) : (
        <div
          ref={box.ref}
          tabIndex={0}
          role="log"
          aria-label="Service logs"
          onScroll={box.onScroll}
          class="h-[65vh] overflow-auto rounded-lg border border-line bg-bg p-3 font-mono text-xs leading-relaxed"
        >
          {lines.length === 0 ? (
            <p class="text-muted">{query ? "No lines match this search in the chosen range." : "There are no log lines in the chosen range yet."}</p>
          ) : (
            lines.map((l, i) => (
              <div key={i} class={`whitespace-pre-wrap break-words ${l.stream === "stderr" ? "text-warn" : ""}`}>
                <span class="select-none text-muted">{clock(toMs(l.ts))} </span>
                {many && l.replica && <span class="select-none text-muted">[{l.replica}] </span>}
                {l.line}
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}

type Series = { step: number; series: Record<string, Points> };
/** Chart lines from series names: pick(d, ["Sent", "net_tx"]). */
const pick = (d: Series, ...lines: [string, string][]) => lines.map(([label, name]) => ({ label, points: d.series[name] ?? [] }));

/** Metrics refresh with each 10-second sample; people other than the owner do not get that event and poll. */
const useMetrics = <T,>(path: string | null) => useApi<T>(path, (e) => e.kind === "metrics", 15000);

export function ServiceMetrics(props: { service: Service }) {
  const [range, setRange] = useState("1h");
  const r = useMetrics<Series>(`/metrics?scope=service:${props.service.id}&names=cpu,memory,net_rx,net_tx,restarts&range=${range}`);
  return (
    <div class="space-y-4">
      <RangeSelect value={range} onChange={setRange} />
      <Loaded r={r}>
        {(d) => (
          <div class="grid gap-4 lg:grid-cols-2">
            <Chart title="CPU (percent of one core)" lines={pick(d, ["CPU", "cpu"])} fmt={fmt.pct} />
            <Chart title="Memory" lines={pick(d, ["Memory", "memory"])} fmt={fmt.bytes} />
            <Chart title="Network" lines={pick(d, ["Received", "net_rx"], ["Sent", "net_tx"])} fmt={fmt.rate} />
            {d.series.restarts?.some((p) => p[1] > 0) && <Chart title="Crashes (per 10 seconds)" lines={pick(d, ["Crashes", "restarts"])} fmt={fmt.count} />}
          </div>
        )}
      </Loaded>
    </div>
  );
}

function RangeSelect(props: { value: string; onChange: (v: string) => void }) {
  return (
    <div class="flex justify-end">
      <Select class="w-auto" aria-label="Time range" value={props.value} onChange={props.onChange} options={RANGES} />
    </div>
  );
}

export function Console(props: { service: Service; replicas: Replica[]; canUse: boolean; label: (r: Replica) => string }) {
  const running = props.replicas.filter((r) => r.state === "running");
  const [container, setContainer] = useState(running[0]?.id ?? "");
  const [state, setState] = useState<"idle" | "connecting" | "open" | "closed">("idle");
  const [error, setError] = useState("");
  const box = useRef<HTMLDivElement>(null);
  const session = useRef<{ close: () => void } | null>(null);
  useEffect(() => () => session.current?.close(), []);

  const connect = async () => {
    session.current?.close();
    session.current = null;
    setError("");
    setState("connecting");
    const path = `/api/services/${props.service.id}/console${qs({ container })}`;
    const el = box.current;
    if (!el) return;
    try {
      const [{ Terminal }, { FitAddon }] = await Promise.all([import("@xterm/xterm"), import("@xterm/addon-fit"), import("@xterm/xterm/css/xterm.css")]);
      el.replaceChildren();
      const term = new Terminal({
        cursorBlink: true,
        fontSize: 13,
        fontFamily: 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
        theme: { background: "#0b1018", foreground: "#e9eef5", cursor: "#ff6a3d", selectionBackground: "#ff6a3d55" },
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(el);
      fit.fit();
      const ws = new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${path}`);
      ws.binaryType = "arraybuffer";
      let opened = false;
      let refused = "";
      const resize = () => ws.readyState === WebSocket.OPEN && ws.send(JSON.stringify({ resize: [term.cols, term.rows] }));
      ws.onopen = () => {
        opened = true;
        setState("open");
        resize();
        term.focus();
      };
      ws.onmessage = (e) => {
        if (typeof e.data !== "string") return term.write(new Uint8Array(e.data));
        // The shell's output is binary; a text message is a refusal ({"error": ...}) before close code 4403.
        try {
          refused = JSON.parse(e.data).error ?? "";
        } catch {
          term.write(e.data);
        }
      };
      ws.onclose = (e) => {
        setState("closed");
        if (refused || e.code === 4403) setError(refused ? sentence(refused) : "The server refused to open a shell.");
        else if (opened) term.write("\r\n\x1b[2mThe session ended.\x1b[0m\r\n");
        // Refusals before the upgrade (like an expired session) reach the browser without a reason.
        else setError("The console could not connect. Check that the container is still running, then try again.");
      };
      const input = term.onData((d) => ws.readyState === WebSocket.OPEN && ws.send(d));
      const resized = term.onResize(resize);
      const ro = new ResizeObserver(() => fit.fit());
      ro.observe(el);
      session.current = {
        close: () => {
          ro.disconnect();
          input.dispose();
          resized.dispose();
          ws.close();
          term.dispose();
        },
      };
    } catch (e) {
      setError(errText(e));
      setState("closed");
    }
  };

  if (!props.canUse) return <Note>A shell can read the service's environment, so opening one needs the operator role on this project and the secrets:read grant.</Note>;
  return (
    <div class="space-y-4">
      <div class="flex flex-wrap items-end gap-3">
        <Field label="Container" class="min-w-48 flex-1">
          <Select
            value={container}
            onChange={setContainer}
            options={running.length ? running.map((r): [string, string] => [r.id, props.label(r)]) : [["", "No running containers"]]}
          />
        </Field>
        <button class="btn btn-primary" disabled={!container || state === "connecting"} onClick={connect}>
          {state === "open" ? "Reconnect" : "Open shell"}
        </button>
      </div>
      <ErrorBox error={error} />
      <div ref={box} class="h-[60vh] overflow-hidden rounded-lg border border-line bg-[#0b1018] p-2">
        {state === "idle" && <p class="p-2 font-mono text-xs text-[#a3afc0]">Choose a container and open a shell. Everything you type runs inside it.</p>}
      </div>
    </div>
  );
}

export interface Now {
  at: number;
  cpu: number;
  cores: number[];
  load: number[];
  memory: { used: number; total: number };
  swap: { used: number; total: number };
  disks: { mount: string; used: number; total: number }[];
  disk_io?: { rx: number; tx: number };
  net: { rx: number; tx: number };
  temp?: number | null;
  gpus: { index: number; name: string; util: number; mem_used: number; mem_total: number; temp: number | null; power: number | null }[];
  services: { id: string; cpu: number; memory: number; net_rx: number; net_tx: number; restarts: number }[];
}

/** One server's current numbers in a compact card, for the dashboard; links to its page. */
export function ServerSummary(props: { node: Node }) {
  const n = props.node;
  const now = useMetrics<Now>(`/metrics/now${qs({ node: n.local ? null : n.id })}`);
  const d = now.data?.at ? now.data : null;
  const disk = d?.disks.find((x) => x.mount === "/") ?? d?.disks[0];
  const stat = (label: string, value: string, sub: string, pct?: number) => {
    const p = pct == null ? null : Math.min(100, Math.max(0, pct));
    return (
      <div class="min-w-0">
        <p class="text-xs text-muted">{label}</p>
        <p class="mt-0.5 truncate font-semibold tabular-nums">{value}</p>
        <p class="truncate text-xs text-muted tabular-nums">{sub}</p>
        {p != null && (
          <div class="mt-1.5 h-1 overflow-hidden rounded-full bg-raised" role="meter" aria-valuenow={Math.round(p)} aria-valuemin={0} aria-valuemax={100} aria-label={label}>
            <div class={`h-full rounded-full ${p >= 90 ? "bg-bad" : p >= 75 ? "bg-warn" : "bg-accent"}`} style={{ width: `${p}%` }} />
          </div>
        )}
      </div>
    );
  };
  return (
    <a href={`/servers/${n.id}`} class="card block p-4 hover:border-accent/60">
      <div class="mb-3 flex items-center gap-2">
        <h2 class="min-w-0 truncate font-semibold">{n.name}</h2>
        {n.address && <span class="truncate font-mono text-xs text-muted">{n.address}</span>}
        {n.status !== "online" && <Pill state={n.status} class="ml-auto" />}
      </div>
      {!d ? (
        <div class="grid grid-cols-2 gap-4 sm:grid-cols-4" aria-hidden="true">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} class="space-y-1.5">
              <Skel class="h-3 w-12" />
              <Skel class="h-4 w-16" />
              <Skel class="h-1 w-full" />
            </div>
          ))}
        </div>
      ) : (
        <div class={`grid grid-cols-2 gap-4 ${d.gpus.length ? "sm:grid-cols-5" : "sm:grid-cols-4"}`}>
          {stat("CPU", fmt.pct(d.cpu), `${d.cores.length} cores · load ${d.load[0]?.toFixed(1) ?? "?"}`, d.cpu)}
          {stat("Memory", bytes(d.memory.used), `of ${bytes(d.memory.total)}`, (100 * d.memory.used) / (d.memory.total || 1))}
          {disk ? stat("Disk", bytes(disk.used), `of ${bytes(disk.total)}`, (100 * disk.used) / (disk.total || 1)) : stat("Disk", "Unknown", "")}
          {stat("Network", `↓ ${fmt.rate(d.net.rx)}`, `↑ ${fmt.rate(d.net.tx)}`)}
          {d.gpus.length > 0 &&
            stat(
              d.gpus.length === 1 ? "GPU" : `${d.gpus.length} GPUs`,
              fmt.pct(d.gpus.reduce((a, g) => a + g.util, 0) / d.gpus.length),
              d.gpus[0].temp != null ? `${Math.round(Math.max(...d.gpus.map((g) => g.temp ?? 0)))} °C` : `${bytes(d.gpus.reduce((a, g) => a + g.mem_used, 0))} memory`,
              d.gpus.reduce((a, g) => a + g.util, 0) / d.gpus.length,
            )}
        </div>
      )}
    </a>
  );
}

/** Live tiles and history charts of one server ("local" is the panel's own). */
export function ServerCharts(props: { node: string }) {
  const node = props.node === "local" ? null : props.node;
  const now = useMetrics<Now>(`/metrics/now${qs({ node })}`);
  const [range, setRange] = useState("1h");
  const disks = now.data?.disks ?? [];
  const names = ["cpu", "memory", "swap", "load1", "disk_read", "disk_write", "net_rx", "net_tx", "temp", ...disks.map((d) => `disk:${d.mount}`)].join(",");
  const host = useMetrics<Series>(now.data ? `/metrics${qs({ scope: "host", names, range, node })}` : null);
  // Other servers answer 404 until their first sample reaches the panel.
  if (now.error?.status === 404) return <Empty>There are no measurements from this server yet. They arrive every 10 seconds while it is online.</Empty>;
  return (
    <Loaded r={now}>
      {(n) =>
        !n.at ? (
          <Empty>Tugport is taking its first measurements, which only takes a few seconds.</Empty>
        ) : (
          <div class="space-y-6">
            {Date.now() / 1000 - n.at > 60 && <Note tone="warn">These are the last measurements this server sent, {ago(n.at)}.</Note>}
            <div class="grid grid-cols-2 gap-3 lg:grid-cols-4">
              <Tile label="CPU" value={fmt.pct(n.cpu)} sub={`${n.cores.length} cores · load ${n.load.map((l) => l.toFixed(2)).join(" ")}`} pct={n.cpu} />
              <Tile label="Memory" value={bytes(n.memory.used)} sub={`of ${bytes(n.memory.total)}`} pct={(100 * n.memory.used) / (n.memory.total || 1)} />
              {n.disks.slice(0, 1).map((d) => (
                <Tile key={d.mount} label={`Disk ${d.mount}`} value={bytes(d.used)} sub={`of ${bytes(d.total)}`} pct={(100 * d.used) / (d.total || 1)} />
              ))}
              <Tile label="Network" value={fmt.rate(n.net.rx)} sub={`in · ${fmt.rate(n.net.tx)} out`} />
            </div>
            <RangeSelect value={range} onChange={setRange} />
            <Loaded r={host}>
              {(h) => (
                <div class="grid gap-4 lg:grid-cols-2">
                  <Chart title="CPU" lines={pick(h, ["CPU", "cpu"])} fmt={fmt.pct} max={100} />
                  <Chart title="Load (1 minute)" lines={pick(h, ["Load", "load1"])} fmt={fmt.count} />
                  <Chart title="Memory" lines={pick(h, ["Memory", "memory"], ["Swap", "swap"])} fmt={fmt.bytes} />
                  <Chart title="Disk space used" lines={pick(h, ...disks.map((d): [string, string] => [d.mount, `disk:${d.mount}`]))} fmt={fmt.pct} max={100} />
                  <Chart title="Disk I/O" lines={pick(h, ["Read", "disk_read"], ["Write", "disk_write"])} fmt={fmt.rate} />
                  <Chart title="Network" lines={pick(h, ["Received", "net_rx"], ["Sent", "net_tx"])} fmt={fmt.rate} />
                  {(h.series.temp?.length ?? 0) > 0 && <Chart title="Temperature" lines={pick(h, ["Temperature", "temp"])} fmt={fmt.celsius} />}
                </div>
              )}
            </Loaded>
            {n.gpus.map((g) => (
              <Gpu key={g.index} gpu={g} range={range} node={node} />
            ))}
          </div>
        )
      }
    </Loaded>
  );
}

function Tile(props: { label: string; value: string; sub?: string; pct?: number }) {
  const pct = props.pct == null ? null : Math.min(100, Math.max(0, props.pct));
  return (
    <div class="card min-w-0 p-4">
      <p class="truncate text-xs text-dim">{props.label}</p>
      <p class="mt-1 text-xl font-semibold">{props.value}</p>
      {props.sub && <p class="truncate text-xs text-muted">{props.sub}</p>}
      {pct != null && (
        <div class="mt-3 h-1.5 overflow-hidden rounded-full bg-raised" role="meter" aria-valuenow={Math.round(pct)} aria-valuemin={0} aria-valuemax={100} aria-label={props.label}>
          <div class={`h-full rounded-full ${pct >= 90 ? "bg-bad" : pct >= 75 ? "bg-warn" : "bg-accent"}`} style={{ width: `${pct}%` }} />
        </div>
      )}
    </div>
  );
}

function Gpu(props: { gpu: Now["gpus"][number]; range: string; node: string | null }) {
  const r = useMetrics<Series>(`/metrics${qs({ scope: `gpu:${props.gpu.index}`, names: "util,memory,temp,power", range: props.range, node: props.node })}`);
  return (
    <Card title={`GPU ${props.gpu.index}: ${props.gpu.name}`}>
      <Loaded r={r}>
        {(d) => (
          <div class="grid gap-4 lg:grid-cols-2">
            <Chart title="Utilization" lines={pick(d, ["Utilization", "util"])} fmt={fmt.pct} max={100} />
            <Chart title="VRAM" lines={pick(d, ["VRAM", "memory"])} fmt={fmt.bytes} />
            <Chart title="Temperature" lines={pick(d, ["Temperature", "temp"])} fmt={fmt.celsius} />
            <Chart title="Power" lines={pick(d, ["Power", "power"])} fmt={fmt.watts} />
          </div>
        )}
      </Loaded>
    </Card>
  );
}

export function DockerUsage() {
  const r = useApi<{ images: number; build_cache: number; volumes: number; containers: number }>("/docker/usage");
  return (
    <Card title="Docker disk usage">
      <Loaded r={r}>
        {(u) => {
          const rows: [string, number][] = [
            ["Images", u.images],
            ["Build cache", u.build_cache],
            ["Volumes", u.volumes],
            ["Containers", u.containers],
          ];
          const total = rows.reduce((a, [, v]) => a + v, 0) || 1;
          return (
            <div class="space-y-3">
              {rows.map(([label, v]) => (
                <div key={label}>
                  <div class="flex justify-between text-sm">
                    <span>{label}</span>
                    <span class="tabular-nums text-dim">{bytes(v)}</span>
                  </div>
                  <div class="mt-1 h-1.5 overflow-hidden rounded-full bg-raised">
                    <div class="h-full rounded-full bg-accent" style={{ width: `${(100 * v) / total}%` }} />
                  </div>
                </div>
              ))}
              <p class="text-xs text-muted">Automatic cleanup removes unused images and caps the build cache, and it never touches volumes.</p>
            </div>
          );
        }}
      </Loaded>
    </Card>
  );
}

export function TugportLog() {
  const [level, setLevel] = useState("info");
  const r = useApi<{ lines: { ts: number | string; level: string; target: string; message: string }[] }>(`/logs/tugport${qs({ limit: 300, level })}`);
  return (
    <Card
      title="Tugport log"
      actions={
        <Select
          class="w-auto"
          aria-label="Lowest level shown"
          value={level}
          onChange={setLevel}
          options={[
            ["error", "Errors"],
            ["warn", "Warnings and up"],
            ["info", "Info and up"],
            ["debug", "Everything"],
          ]}
        />
      }
    >
      <Loaded r={r}>
        {(d) => (
          <div class="h-80 overflow-auto rounded-md border border-line bg-bg p-2 font-mono text-xs leading-relaxed">
            {d.lines.length === 0 && <p class="text-muted">There is nothing at this level yet.</p>}
            {d.lines.map((l, i) => {
              const level = String(l.level).toLowerCase();
              return (
                <div key={i} class={`whitespace-pre-wrap break-words ${level === "error" ? "text-bad" : level === "warn" ? "text-warn" : ""}`}>
                  <span class="select-none text-muted">{clock(toMs(l.ts))} </span>
                  {l.message}
                </div>
              );
            })}
          </div>
        )}
      </Loaded>
    </Card>
  );
}
