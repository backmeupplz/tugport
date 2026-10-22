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
  pct: (v: number) => `${v < 10 ? v.toFixed(1) : Math.round(v)}%`,
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

export function Chart(props: { title: string; lines: Line[]; fmt: (v: number) => string; max?: number }) {
  const box = useRef<HTMLDivElement>(null);
  const tick = useThemeTick();
  const lines = props.lines.slice(0, 3); // a fourth series would need a fourth validated color
  const signature = lines.map((l) => `${l.label}:${l.points.length}:${l.points[l.points.length - 1]?.join(",")}`).join("|");
  const hasData = lines.some((l) => l.points.length > 0);
  const first = lines[0]?.points ?? [];
  const now = first.length ? first[first.length - 1][1] : null;
  const peak = first.length ? Math.max(...first.map((p) => p[1])) : null;

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
          legend: { live: true },
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
              value: (_u: unknown, v: number | null) => (v == null ? "–" : props.fmt(v)),
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
    <section class="card min-w-0 p-4" aria-label={`${props.title} chart`}>
      <header class="mb-2 flex flex-wrap items-baseline justify-between gap-2">
        <h3 class="text-sm font-medium">{props.title}</h3>
        {now != null && peak != null && (
          <span class="text-xs text-muted">
            now {props.fmt(now)} · peak {props.fmt(peak)}
          </span>
        )}
      </header>
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
