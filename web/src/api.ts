// The API client, the shared event stream and the data hooks every page uses.
import { createContext } from "preact";
import { useCallback, useContext, useEffect, useRef, useState } from "preact/hooks";

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

let unauthorized = () => {};
/** Called when a signed-in request gets a 401 (the session expired or was revoked). */
export const onUnauthorized = (fn: () => void) => {
  unauthorized = fn;
};

export async function api<T = any>(method: string, path: string, body?: unknown): Promise<T> {
  let res: Response;
  try {
    res = await fetch("/api" + path, {
      method,
      credentials: "same-origin",
      headers: body === undefined ? undefined : { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch {
    throw new ApiError(0, "Tugport is not reachable right now; check the connection and try again.");
  }
  const text = await res.text().catch(() => "");
  let data: any = {};
  try {
    data = text ? JSON.parse(text) : {};
  } catch {
    // Every API answer is JSON; anything else (a proxy's error page) gets its status line below.
  }
  if (!res.ok) {
    const message = typeof data?.error === "string" && data.error ? data.error : `${res.status} ${res.statusText}`;
    if (res.status === 401 && !path.startsWith("/auth/") && !path.startsWith("/setup")) unauthorized();
    throw new ApiError(res.status, message);
  }
  return data as T;
}

export const get = <T = any>(p: string) => api<T>("GET", p);
export const post = <T = any>(p: string, body: unknown = {}) => api<T>("POST", p, body);
export const put = <T = any>(p: string, body: unknown) => api<T>("PUT", p, body);
export const patch = <T = any>(p: string, body: unknown) => api<T>("PATCH", p, body);
export const del = <T = any>(p: string, body?: unknown) => api<T>("DELETE", p, body);

/** Endpoints marked "planned" in docs/API.md answer 404 until a later build implements them. */
export const isPlanned = (e: unknown) => e instanceof ApiError && e.status === 404;

// ---- Types (only the fields the panel reads) ----

export type Role = "viewer" | "deployer" | "operator" | "admin";
export const ROLES: Role[] = ["viewer", "deployer", "operator", "admin"];
export const ROLE_HELP: Record<Role, string> = {
  viewer: "A viewer sees status, charts, logs and the names of environment variables.",
  deployer: "A deployer can also deploy, restart, stop, start and roll back.",
  operator: "An operator can also change environment, domains, mounts and settings, and create services.",
  admin: "An admin can also delete things and manage project members.",
};
const rank = (r: string | null | undefined) => (r === "owner" ? 5 : ROLES.indexOf(r as Role) + 1);
export const atLeast = (r: string | null | undefined, need: Role) => rank(r) >= rank(need);

export interface Principal {
  user_id: string;
  name: string;
  kind: "human" | "agent";
  is_owner: boolean;
  all_projects_role: Role | null;
  project_roles: Record<string, Role>;
  grants: string[];
}
export interface Me {
  principal: Principal;
  email: string | null;
  totp: boolean;
}
export const MeContext = createContext<Me | null>(null);
/** The signed-in user. Only rendered inside the signed-in shell, which always provides it. */
export function useMe(): Me {
  const me = useContext(MeContext);
  if (!me) throw new Error("useMe outside the signed-in shell");
  return me;
}
export const hasGrant = (me: Me, g: string) => me.principal.is_owner || me.principal.grants.includes(g);

export interface Project {
  id: string;
  name: string;
  description: string | null;
  services: number;
  /** How many of its services are in each state, like {running: 2, stopped: 1}. */
  states?: Record<string, number>;
  role: Role | null;
  created_at: number;
}
export interface Replica {
  id: string;
  name: string;
  deploy: string;
  replica: number;
  state: string;
  health: string | null;
  ip: string | null;
  image: string;
  created: number;
  node: string;
  compose_service: string | null;
}
export interface Status {
  state: string;
  running: number;
  desired: number;
  live_deploy: string | null;
  replicas: Replica[];
}
export type Source =
  | { type: "git"; provider?: string | null; repo: string; branch: string; path: string; watch_paths: string[]; auto_deploy: boolean }
  | { type: "image"; image: string }
  | { type: "dockerfile"; content: string }
  | { type: "compose"; yaml: string };
export type Spec = Record<string, any> & { source: Source; replicas: number };
export interface Service {
  id: string;
  project_id: string;
  project: string;
  name: string;
  kind: "app" | "database" | "compose";
  spec: Spec;
  created_at: number;
  updated_at: number;
}
export interface Deploy {
  id: string;
  service_id: string;
  status: string;
  trigger: string;
  actor: string | null;
  ref: string | null;
  commit_sha: string | null;
  commit_message: string | null;
  image: string | null;
  error: string | null;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
}
export interface Domain {
  id: string;
  service_id: string;
  host: string;
  path: string;
  port: number;
  container: string | null;
  https: boolean;
  via_tunnel: boolean;
  options: Record<string, any>;
}
export interface ServiceDetail {
  service: Service;
  status: Status;
  domains: Domain[];
  env: string[];
  files: string[];
  last_deploy: Deploy | null;
  /** Ports the image declares with EXPOSE, as of its last deploy. */
  exposed_ports?: number[];
  role: Role | null;
}
export interface User {
  id: string;
  kind: "human" | "agent";
  name: string;
  email?: string | null;
  description?: string | null;
  is_owner?: boolean;
  all_projects_role?: Role | null;
  projects?: Record<string, Role>;
  grants?: string[];
  disabled?: boolean;
  totp?: boolean;
  created_at?: number;
}
export interface Key {
  id: string;
  name: string;
  prefix: string;
  expires_at: number | null;
  allowed_ips: string[];
  last_used_at: number | null;
  last_used_ip: string | null;
  created_at: number;
  revoked_at: number | null;
}
export type Snippets = Record<"env" | "cli" | "claude_code" | "codex" | "curl", string>;
export interface Node {
  id: string;
  name: string;
  local: boolean;
  mesh_ip: string | null;
  region: string | null;
  labels: Record<string, string>;
  status: "online" | "unreachable" | "offline" | "joining";
  last_seen_at: number | null;
  created_at: number;
  info: {
    version?: string;
    docker?: string | null;
    cpus?: number;
    memory_total?: number;
    arch?: string;
    nvidia?: boolean;
    dri?: boolean;
  };
}

/** Listing servers needs the owner or a role on all projects. */
export const canSeeServers = (me: Me) => me.principal.is_owner || !!me.principal.all_projects_role;

/** Every server, for people allowed to list them; null for everyone else. */
export function useNodes() {
  const r = useApi<{ mesh: boolean; nodes: Node[] }>(canSeeServers(useMe()) ? "/nodes" : null, (e) => e.kind === "nodes");
  return r.data ?? null;
}

/** A server's name for display. Without the server list only the panel's own server is known. */
export const nodeName = (nodes: { nodes: Node[] } | null, id: string) =>
  nodes?.nodes.find((n) => n.id === id)?.name ?? (id === "local" ? "the panel's server" : "another server");

export const FINAL = new Set(["live", "failed", "cancelled", "superseded"]);

// ---- The shared event stream (/api/events) ----

export interface Event {
  kind: string;
  project_id?: string;
  service_id?: string;
  data: any;
  at: number;
}
const KINDS = ["deploy", "deploy.log", "service.status", "service.updated", "audit", "certificate", "settings", "metrics", "warning", "nodes"];
const listeners = new Set<(e: Event) => void>();
let source: EventSource | null = null;
let wanted = false;
/** The stream was open before, so a later open (a reconnect, a tab shown again) may have missed events. */
let wasOpen = false;

export function startEvents() {
  wanted = true;
  if (source) return;
  const s = new EventSource("/api/events");
  source = s;
  s.onopen = () => {
    if (wasOpen) listeners.forEach((f) => f({ kind: "resync", data: {}, at: Date.now() }));
    wasOpen = true;
  };
  const fire = (m: MessageEvent) => {
    let e: Event;
    try {
      e = JSON.parse(m.data);
    } catch {
      return;
    }
    listeners.forEach((f) => f(e));
  };
  for (const k of KINDS) s.addEventListener(k, fire);
  s.onerror = () => {
    // The browser retries dropped connections itself; a refused one (like a 401) stays closed.
    if (s.readyState !== EventSource.CLOSED) return;
    source = null;
    if (wanted) setTimeout(startEvents, 5000);
  };
}

export function stopEvents() {
  wanted = false;
  wasOpen = false;
  source?.close();
  source = null;
}

// Browsers allow only six HTTP/1.1 connections per host, and every open tab would hold one
// for the stream. Hidden tabs let theirs go and catch up with a "resync" when shown again.
document.addEventListener("visibilitychange", () => {
  if (!wanted) return;
  if (document.visibilityState === "hidden") {
    source?.close();
    source = null;
  } else if (!source) {
    startEvents();
  }
});

export function useEvent(fn: (e: Event) => void) {
  const ref = useRef(fn);
  ref.current = fn;
  useEffect(() => {
    const f = (e: Event) => ref.current(e);
    listeners.add(f);
    return () => {
      listeners.delete(f);
    };
  }, []);
}

/**
 * GET `path` (null = skip). With `live`, matching events refetch the data. With `poll` (ms), it
 * also refetches after that long without a refetch, for data the stream does not bring to
 * everyone: "metrics" and "nodes" events reach only the owner.
 */
export function useApi<T = any>(path: string | null, live?: (e: Event) => boolean, poll?: number) {
  const [state, setState] = useState<{ path?: string | null; data?: T; error?: ApiError }>({});
  const latest = useRef(path);
  latest.current = path;
  const reload = useCallback(async () => {
    if (!path) return;
    try {
      const data = await get<T>(path);
      if (latest.current === path) setState({ path, data });
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      if (latest.current === path) setState((s) => ({ path, data: s.path === path ? s.data : undefined, error }));
    }
  }, [path]);
  useEffect(() => {
    reload();
  }, [reload]);

  const liveRef = useRef(live);
  liveRef.current = live;
  const timer = useRef<ReturnType<typeof setTimeout>>();
  useEvent((e) => {
    if (!liveRef.current || (e.kind !== "resync" && !liveRef.current(e))) return;
    clearTimeout(timer.current);
    timer.current = setTimeout(reload, 300);
  });
  useEffect(() => () => clearTimeout(timer.current), []);
  // Every refetch re-arms the timer, so events that keep the data fresh leave nothing to poll.
  // A hidden tab lets it lapse and catches up through the "resync" event.
  useEffect(() => {
    if (!poll || !path) return;
    const t = setTimeout(() => document.visibilityState === "visible" && reload(), poll);
    return () => clearTimeout(t);
  }, [state, poll, reload]);

  const mine = state.path === path;
  return {
    data: mine ? state.data : undefined,
    error: mine ? state.error : undefined,
    loading: !!path && !(mine && (state.data !== undefined || state.error)),
    reload,
    set: (data: T) => setState({ path, data }),
  };
}
