// Time-series charts on uPlot, loaded on first use. Colors come from the theme tokens
// (--series-1..3 were validated for colorblind separation on both surfaces).
import { useEffect, useRef, useState } from "preact/hooks";
import { bytes } from "./util";

export type Points = [number, number][];
export interface Line {
  label: string;
  points: Points;
}

export const fmt = {
  pct: (v: number) => (v > 0 && v < 0.01 ? "<0.01%" : `${v > 0 && v < 1 ? v.toFixed(2) : v < 10 ? v.toFixed(1) : Math.round(v)}%`),
  bytes: (v: number) => bytes(v),
  rate: (v: number) => `${bytes(v)}/s`,
  count: (v: number) => (Math.abs(v) >= 1000 ? `${(v / 1000).toFixed(1)}K` : `${Math.round(v * 100) / 100}`),
  celsius: (v: number) => `${Math.round(v)} °C`,
  watts: (v: number) => `${Math.round(v)} W`,
};

/** Epoch seconds or milliseconds -> seconds (uPlot's time unit). */
const toSec = (t: number) => (t > 1e12 ? t / 1000 : t);

function cssColor(el: HTMLElement, token: string): string {
  const probe = document.createElement("span");
  probe.style.color = `var(${token})`;
  el.appendChild(probe);
  const c = getComputedStyle(probe).color;
  probe.remove();
  return c;
}
const withAlpha = (rgb: string, a: number) => {
  const [r, g, b] = rgb.match(/[\d.]+/g) ?? ["0", "0", "0"];
  return `rgba(${r}, ${g}, ${b}, ${a})`;
};

/** Bumps when the theme changes (manual toggle or system setting) so charts repaint. */
function useThemeTick() {
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const bump = () => setTick((t) => t + 1);
    const mq = matchMedia("(prefers-color-scheme: dark)");
    mq.addEventListener("change", bump);
    window.addEventListener("tugport-theme", bump);
    return () => {
      mq.removeEventListener("change", bump);
      window.removeEventListener("tugport-theme", bump);
    };
  }, []);
  return tick;
}

/** Seconds -> "8:05 PM" for the hover readout. */
const at = (t: number) => new Date(t * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });

export function Chart(props: { title: string; lines: Line[]; fmt: (v: number) => string; max?: number }) {
  const box = useRef<HTMLDivElement>(null);
  const tick = useThemeTick();
  // The values under the cursor, which replace the summary while pointing at the chart.
  const [hover, setHover] = useState<string | null>(null);
  const lines = props.lines.slice(0, 3); // a fourth series would need a fourth validated color
  const signature = lines.map((l) => `${l.label}:${l.points.length}:${l.points[l.points.length - 1]?.join(",")}`).join("|");
  const hasData = lines.some((l) => l.points.length > 0);
  const last = (p: Points) => p[p.length - 1][1];
  // One series: its current value and peak. Several: each one's current value, named.
  const summary =
    hover ??
    (lines.length === 1
      ? lines[0].points.length
        ? `now ${props.fmt(last(lines[0].points))} · peak ${props.fmt(Math.max(...lines[0].points.map((p) => p[1])))}`
        : ""
      : lines
          .filter((l) => l.points.length)
          .map((l) => `${l.label} ${props.fmt(last(l.points))}`)
          .join(" · "));

  useEffect(() => {
    const el = box.current;
    if (!el || !hasData) return;
    let plot: { destroy: () => void; setSize: (s: { width: number; height: number }) => void } | undefined;
    let ro: ResizeObserver | undefined;
    let dead = false;
    (async () => {
      const [{ default: uPlot }] = await Promise.all([import("uplot"), import("uplot/dist/uPlot.min.css")]);
      if (dead) return;
      const xs = [...new Set(lines.flatMap((l) => l.points.map((p) => p[0])))].sort((a, b) => a - b);
      const data = [xs.map(toSec), ...lines.map((l) => new Map(l.points)).map((m) => xs.map((x) => m.get(x) ?? null))];
      const ink = cssColor(el, "--muted");
      const grid = cssColor(el, "--line");
      const colors = ["--series-1", "--series-2", "--series-3"].map((t) => cssColor(el, t));
      const font = "11px system-ui, sans-serif";
      plot = new uPlot(
        {
          width: el.clientWidth,
          height: 170,
          cursor: { points: { size: 8 } },
          legend: { show: false },
          hooks: {
            setCursor: [
              (u: { cursor: { idx?: number | null } }) => {
                const i = u.cursor.idx;
                if (i == null) return setHover(null);
                const vals = lines.map((l, k) => {
                  const v = data[k + 1][i];
                  return `${lines.length > 1 ? `${l.label} ` : ""}${v == null ? "–" : props.fmt(v)}`;
                });
                setHover(`${at(data[0][i] ?? 0)} · ${vals.join(" · ")}`);
              },
            ],
          },
          scales: { y: { range: (_u: unknown, _min: number, max: number) => [0, props.max ?? (max > 0 ? max * 1.15 : 1)] } },
          axes: [
            { stroke: ink, font, grid: { stroke: grid, width: 1 }, ticks: { stroke: grid, width: 1 } },
            { stroke: ink, font, size: 64, grid: { stroke: grid, width: 1 }, ticks: { show: false }, values: (_u: unknown, vals: number[]) => vals.map(props.fmt) },
          ],
          series: [
            {},
            ...lines.map((l, i) => ({
              label: l.label,
              stroke: colors[i],
              width: 2,
              fill: lines.length === 1 ? withAlpha(colors[i], 0.1) : undefined,
              points: { show: false },
            })),
          ],
        },
        data as any,
        el,
      );
      ro = new ResizeObserver(() => plot?.setSize({ width: el.clientWidth, height: 170 }));
      ro.observe(el);
    })();
    return () => {
      dead = true;
      ro?.disconnect();
      plot?.destroy();
    };
  }, [signature, tick, hasData]);

  return (
    <section class="card min-w-0 p-5" aria-label={`${props.title} chart`}>
      <header class="mb-3 flex flex-wrap items-baseline justify-between gap-2">
        <h3 class="text-sm font-semibold">{props.title}</h3>
        {summary && <span class="text-xs text-muted tabular-nums">{summary}</span>}
      </header>
      {lines.length > 1 && (
        <p class="-mt-1 mb-2 flex gap-3 text-xs text-dim">
          {lines.map((l, i) => (
            <span key={l.label} class="flex items-center gap-1.5">
              <span class="h-0.5 w-3 rounded-full" style={{ background: `var(--series-${i + 1})` }} aria-hidden="true" />
              {l.label}
            </span>
          ))}
        </p>
      )}
      {hasData ? <div ref={box} /> : <p class="py-12 text-center text-sm text-muted">There is no data for this range yet.</p>}
    </section>
  );
}

export const RANGES: [string, string][] = [
  ["1h", "Last hour"],
  ["6h", "Last 6 hours"],
  ["24h", "Last 24 hours"],
  ["7d", "Last 7 days"],
  ["30d", "Last 30 days"],
];
