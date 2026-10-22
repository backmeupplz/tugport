// Formatting and small pure helpers shared by every page.

/** "wrong name or password" -> "Wrong name or password." */
export function sentence(msg: string): string {
  const s = msg.trim();
  if (!s) return s;
  const cap = s[0].toUpperCase() + s.slice(1);
  return /[.!?]$/.test(cap) ? cap : cap + ".";
}

const isObj = (x: unknown): x is Record<string, unknown> => x !== null && typeof x === "object" && !Array.isArray(x);
const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/** RFC 7386 JSON merge patch that turns `a` into `b`. Arrays are replaced whole; removed keys become null. */
export function mergePatch(a: Record<string, unknown>, b: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const k of Object.keys(a)) if (b[k] === undefined) out[k] = null;
  for (const [k, v] of Object.entries(b)) {
    if (v === undefined) continue;
    const old = a[k];
    if (isObj(old) && isObj(v)) {
      const p = mergePatch(old, v);
      if (Object.keys(p).length) out[k] = p;
    } else if (!same(old, v)) out[k] = v;
  }
  return out;
}

/** KEY=value lines from a .env file; comments, blank lines and `export ` prefixes are ignored. */
export function parseEnv(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim().replace(/^export\s+/, "");
    const eq = line.indexOf("=");
    if (!line || line.startsWith("#") || eq < 1) continue;
    let v = line.slice(eq + 1).trim();
    const q = v[0];
    if ((q === '"' || q === "'") && v.length > 1 && v.endsWith(q)) {
      v = v.slice(1, -1);
      if (q === '"') v = v.replace(/\\n/g, "\n").replace(/\\"/g, '"');
    }
    out[line.slice(0, eq).trim()] = v;
  }
  return out;
}

export const validEnvKey = (k: string) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(k);

/** Shell-like split for commands: quotes and backslashes group words. */
export function splitArgs(s: string): string[] {
  const out: string[] = [];
  let cur = "";
  let started = false;
  let quote = "";
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (quote) {
      if (c === quote) quote = "";
      else if (c === "\\" && quote === '"' && i + 1 < s.length) cur += s[++i];
      else cur += c;
    } else if (c === '"' || c === "'") {
      quote = c;
      started = true;
    } else if (c === "\\" && i + 1 < s.length) {
      cur += s[++i];
      started = true;
    } else if (/\s/.test(c)) {
      if (started) out.push(cur);
      cur = "";
      started = false;
    } else {
      cur += c;
      started = true;
    }
  }
  if (started) out.push(cur);
  return out;
}

export const joinArgs = (a: string[]) =>
  a.map((x) => (/^[\w@%+=:,./-]+$/.test(x) ? x : `'${x.replace(/'/g, `'\\''`)}'`)).join(" ");

/** One entry per line, trimmed, blanks dropped. */
export const lines = (s: string) => s.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
/** Entries separated by commas, spaces or newlines. */
export const words = (s: string) => s.split(/[\s,]+/).filter(Boolean);
/** "10.0.0.7" -> "10.0.0.7/32": the API takes CIDR ranges only. */
export const cidr = (s: string) => (s.includes("/") ? s : s.includes(":") ? `${s}/128` : `${s}/32`);
/** "" -> null, "12" -> 12 (number inputs map to optional numbers). */
export const num = (s: string | number | null | undefined) => (s === "" || s == null || isNaN(Number(s)) ? null : Number(s));

const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
const STEPS: [number, Intl.RelativeTimeFormatUnit][] = [
  [60, "second"],
  [3600, "minute"],
  [86400, "hour"],
  [86400 * 30, "day"],
  [86400 * 365, "month"],
  [Infinity, "year"],
];
const UNIT = { second: 1, minute: 60, hour: 3600, day: 86400, month: 86400 * 30, year: 86400 * 365 } as Record<string, number>;

/** Relative time for an epoch-seconds timestamp: "5 minutes ago". */
export function ago(sec: number | null | undefined): string {
  if (!sec) return "never";
  const d = sec - Date.now() / 1000;
  const [, unit] = STEPS.find(([limit]) => Math.abs(d) < limit) ?? STEPS[STEPS.length - 1];
  return rtf.format(Math.round(d / UNIT[unit]), unit);
}

export const when = (sec: number | null | undefined) =>
  sec ? new Date(sec * 1000).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" }) : "";

export const clock = (ms: number) => new Date(ms).toLocaleTimeString(undefined, { hour12: false });

export function duration(sec: number): string {
  if (sec < 60) return `${Math.max(0, Math.round(sec))}s`;
  const m = Math.floor(sec / 60);
  return m < 60 ? `${m}m ${Math.round(sec % 60)}s` : `${Math.floor(m / 60)}h ${m % 60}m`;
}

export function bytes(n: number | null | undefined): string {
  if (n == null || isNaN(n)) return "–";
  const units = ["B", "KB", "MB", "GB", "TB", "PB"];
  let i = 0;
  let v = Math.abs(n);
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${n < 0 ? "-" : ""}${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

/** "?a=1&b=x", skipping empty values. */
export function qs(params: Record<string, string | number | null | undefined>): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== "" && v != null) p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

/** Clipboard write that also works on plain-HTTP panels, where navigator.clipboard is missing. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Fall through to the selection-based copy below.
  }
  const t = document.createElement("textarea");
  t.value = text;
  t.setAttribute("readonly", "");
  t.style.position = "fixed";
  t.style.opacity = "0";
  // An open modal makes the rest of the page inert, so the helper must live inside it.
  (document.querySelector("dialog[open]") ?? document.body).appendChild(t);
  t.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  t.remove();
  return ok;
}
