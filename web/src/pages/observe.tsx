// Logs, charts and the console: everything built on the endpoints docs/API.md marks as planned.
// Each one shows a calm "later build" state while the server still answers 404.
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { get, isPlanned, useApi, type Replica, type Service } from "../api";
import { Chart, RANGES, fmt, type Points } from "../charts";
import { Card, Empty, ErrorBox, Field, Loaded, Loading, Note, Page, Planned, Select, errText } from "../ui";
import { bytes, clock, qs, toMs } from "../util";

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

export function Logs(props: { service: Service; replicas: Replica[] }) {
  const id = props.service.id;
  const [q, setQ] = useState("");
  const [query, setQuery] = useState("");
  const [range, setRange] = useState("1h");
  const [replica, setReplica] = useState("");
  const [live, setLive] = useState(true);
  const [lines, setLines] = useState<LogLine[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const box = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

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
    const es = new EventSource(`/api/services/${id}/logs/stream`);
    const needle = query.toLowerCase();
    let buf: LogLine[] = [];
    let frame = 0;
    es.addEventListener("line", (m) => {
      let l: LogLine;
      try {
        l = JSON.parse(m.data);
      } catch {
        return;
      }
      if ((replica && l.replica !== replica) || (needle && !l.line.toLowerCase().includes(needle))) return;
      buf.push(l);
      if (!frame)
        frame = requestAnimationFrame(() => {
          frame = 0;
          const chunk = buf;
          buf = [];
          setLines((cur) => (cur ?? []).concat(chunk).slice(-MAX_LINES));
        });
    });
    return () => {
      es.close();
      cancelAnimationFrame(frame);
    };
  }, [id, live, ready, query, replica]);

  useLayoutEffect(() => {
    const b = box.current;
    if (b && stick.current) b.scrollTop = b.scrollHeight;
  }, [lines]);

  if (isPlanned(error)) return <Planned />;
  const many = new Set(props.replicas.map((r) => r.name)).size > 1;
  return (
    <div class="space-y-4">
      <div class="flex flex-wrap items-end gap-3">
        <Field label="Search" class="min-w-48 flex-1">
          <input class="input" type="search" placeholder="Text to find, any case" value={q} onInput={(e) => setQ(e.currentTarget.value)} />
        </Field>
        <Field label="Time range">
          <Select value={range} onChange={setRange} options={LOG_RANGES.map(([v, l]): [string, string] => [v, l])} />
        </Field>
        {props.replicas.length > 1 && (
          <Field label="Replica">
            <Select value={replica} onChange={setReplica} options={[["", "All replicas"], ...props.replicas.map((r): [string, string] => [r.name, r.compose_service ?? r.name])]} />
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
          ref={box}
          tabIndex={0}
          role="log"
          aria-label="Service logs"
          onScroll={(e) => {
            const b = e.currentTarget;
            stick.current = b.scrollHeight - b.scrollTop - b.clientHeight < 40;
          }}
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

export function ServiceMetrics(props: { service: Service }) {
  const [range, setRange] = useState("1h");
  const r = useApi<Series>(`/metrics?scope=service:${props.service.id}&names=cpu,memory,net_rx,net_tx,restarts&range=${range}`, (e) => e.kind === "metrics");
  return (
    <div class="space-y-4">
      <RangeSelect value={range} onChange={setRange} />
      <Loaded r={r} planned>
        {(d) => (
          <div class="grid gap-4 lg:grid-cols-2">
            <Chart title="CPU (percent of one core)" lines={[{ label: "CPU", points: d.series.cpu ?? [] }]} fmt={fmt.pct} />
            <Chart title="Memory" lines={[{ label: "Memory", points: d.series.memory ?? [] }]} fmt={fmt.bytes} />
            <Chart
              title="Network"
              lines={[
                { label: "Received", points: d.series.net_rx ?? [] },
                { label: "Sent", points: d.series.net_tx ?? [] },
              ]}
              fmt={fmt.rate}
            />
            {(d.series.restarts?.length ?? 0) > 0 && <Chart title="Restarts" lines={[{ label: "Restarts", points: d.series.restarts ?? [] }]} fmt={fmt.count} />}
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

export function Console(props: { service: Service; replicas: Replica[]; canUse: boolean }) {
  const running = props.replicas.filter((r) => r.state === "running");
  const [container, setContainer] = useState(running[0]?.id ?? "");
  const [state, setState] = useState<"idle" | "planned" | "connecting" | "open" | "closed">("idle");
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
    // A plain GET tells a missing endpoint (404) apart from one that only speaks WebSocket.
    const probe = await fetch(path, { credentials: "same-origin" }).catch(() => null);
    if (probe?.status === 404) {
      setState("planned");
      return;
    }
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
      const resize = () => ws.readyState === WebSocket.OPEN && ws.send(JSON.stringify({ resize: [term.cols, term.rows] }));
      ws.onopen = () => {
        opened = true;
        setState("open");
        resize();
        term.focus();
      };
      ws.onmessage = (e) => term.write(typeof e.data === "string" ? e.data : new Uint8Array(e.data));
      ws.onclose = () => {
        setState("closed");
        if (opened) term.write("\r\n\x1b[2mThe session ended.\x1b[0m\r\n");
        // The server refuses before the upgrade, so the browser never sees the reason.
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
  if (state === "planned") return <Planned />;
  return (
    <div class="space-y-4">
      <div class="flex flex-wrap items-end gap-3">
        <Field label="Container" class="min-w-48 flex-1">
          <Select
            value={container}
            onChange={setContainer}
            options={running.length ? running.map((r): [string, string] => [r.id, r.compose_service ?? r.name]) : [["", "No running containers"]]}
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

export function Server(_: { path?: string }) {
  const now = useApi<Now>("/metrics/now", (e) => e.kind === "metrics");
  const [range, setRange] = useState("1h");
  const disks = now.data?.disks ?? [];
  const names = ["cpu", "memory", "swap", "load1", "disk_read", "disk_write", "net_rx", "net_tx", "temp", ...disks.map((d) => `disk:${d.mount}`)].join(",");
  const host = useApi<Series>(now.data ? `/metrics${qs({ scope: "host", names, range })}` : null, (e) => e.kind === "metrics");
  return (
    <Page title="Server" sub="This machine, its containers and Tugport itself.">
      <div class="space-y-6">
        <Loaded r={now} planned>
          {(n) =>
            !n.at ? (
              <Empty>Tugport is taking its first measurements, which only takes a few seconds.</Empty>
            ) : (
            <>
              <div class="grid grid-cols-2 gap-3 lg:grid-cols-4">
                <Tile label="CPU" value={fmt.pct(n.cpu)} sub={`${n.cores.length} cores · load ${n.load.map((l) => l.toFixed(2)).join(" ")}`} pct={n.cpu} />
                <Tile label="Memory" value={bytes(n.memory.used)} sub={`of ${bytes(n.memory.total)}`} pct={(100 * n.memory.used) / (n.memory.total || 1)} />
                {n.disks.slice(0, 1).map((d) => (
                  <Tile key={d.mount} label={`Disk ${d.mount}`} value={bytes(d.used)} sub={`of ${bytes(d.total)}`} pct={(100 * d.used) / (d.total || 1)} />
                ))}
                <Tile label="Network" value={fmt.rate(n.net.rx)} sub={`in · ${fmt.rate(n.net.tx)} out`} />
              </div>
              <RangeSelect value={range} onChange={setRange} />
              <Loaded r={host} planned>
                {(h) => (
                  <div class="grid gap-4 lg:grid-cols-2">
                    <Chart title="CPU" lines={[{ label: "CPU", points: h.series.cpu ?? [] }]} fmt={fmt.pct} max={100} />
                    <Chart title="Load (1 minute)" lines={[{ label: "Load", points: h.series.load1 ?? [] }]} fmt={fmt.count} />
                    <Chart
                      title="Memory"
                      lines={[
                        { label: "Memory", points: h.series.memory ?? [] },
                        { label: "Swap", points: h.series.swap ?? [] },
                      ]}
                      fmt={fmt.bytes}
                    />
                    <Chart title="Disk space used" lines={disks.map((d) => ({ label: d.mount, points: h.series[`disk:${d.mount}`] ?? [] }))} fmt={fmt.pct} max={100} />
                    <Chart
                      title="Disk I/O"
                      lines={[
                        { label: "Read", points: h.series.disk_read ?? [] },
                        { label: "Write", points: h.series.disk_write ?? [] },
                      ]}
                      fmt={fmt.rate}
                    />
                    <Chart
                      title="Network"
                      lines={[
                        { label: "Received", points: h.series.net_rx ?? [] },
                        { label: "Sent", points: h.series.net_tx ?? [] },
                      ]}
                      fmt={fmt.rate}
                    />
                    {(h.series.temp?.length ?? 0) > 0 && <Chart title="Temperature" lines={[{ label: "Temperature", points: h.series.temp ?? [] }]} fmt={fmt.celsius} />}
                  </div>
                )}
              </Loaded>
              {n.gpus.map((g) => (
                <Gpu key={g.index} index={g.index} gpu={g} range={range} />
              ))}
            </>
            )
          }
        </Loaded>
        <div class="grid gap-6 lg:grid-cols-2">
          <DockerUsage />
          <TugportLog />
        </div>
      </div>
    </Page>
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

function Gpu(props: { index: number; gpu: Now["gpus"][number]; range: string }) {
  const r = useApi<Series>(`/metrics${qs({ scope: `gpu:${props.index}`, names: "util,memory,temp,power", range: props.range })}`);
  return (
    <Card title={`GPU ${props.index}: ${props.gpu.name}`}>
      <Loaded r={r} planned>
        {(d) => (
          <div class="grid gap-4 lg:grid-cols-2">
            <Chart title="Utilization" lines={[{ label: "Utilization", points: d.series.util ?? [] }]} fmt={fmt.pct} max={100} />
            <Chart title="VRAM" lines={[{ label: "VRAM", points: d.series.memory ?? [] }]} fmt={fmt.bytes} />
            <Chart title="Temperature" lines={[{ label: "Temperature", points: d.series.temp ?? [] }]} fmt={fmt.celsius} />
            <Chart title="Power" lines={[{ label: "Power", points: d.series.power ?? [] }]} fmt={fmt.watts} />
          </div>
        )}
      </Loaded>
    </Card>
  );
}

function DockerUsage() {
  const r = useApi<{ images: number; build_cache: number; volumes: number; containers: number }>("/docker/usage");
  return (
    <Card title="Docker disk usage">
      <Loaded r={r} planned>
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

function TugportLog() {
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
      <Loaded r={r} planned>
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
