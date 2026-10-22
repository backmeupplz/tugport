import { useState } from "preact/hooks";
import { atLeast, del, patch, post, type Domain } from "../api";
import { Action, Card, Check, Empty, ErrorBox, Field, Modal, Select, useSubmit } from "../ui";
import { lines, num } from "../util";
import type { Ctx } from "./service";

interface Form {
  host: string;
  path: string;
  port: string;
  container: string;
  https: boolean;
  via_tunnel: boolean;
  www: string;
  redirects: { from: string; to: string; permanent: boolean }[];
  basic: { user: string; password: string; hash?: string }[];
  faUrl: string;
  faHeaders: string;
  strip: boolean;
  rwFrom: string;
  rwTo: string;
  maint: boolean;
  maintHtml: string;
  timeout: string;
  maxBody: string;
  errorPages: Record<string, string>;
}

const MAINTENANCE = "<!doctype html>\n<title>Maintenance</title>\n<h1>We will be back soon.</h1>\n<p>This site is down for planned maintenance.</p>";

function toForm(d?: Domain): Form {
  const o = d?.options ?? {};
  return {
    host: d?.host ?? "",
    path: d?.path ?? "/",
    port: d ? String(d.port) : "",
    container: d?.container ?? "",
    https: d?.https ?? true,
    via_tunnel: d?.via_tunnel ?? false,
    www: o.www_redirect ?? "",
    redirects: o.redirects ?? [],
    basic: (o.basic_auth ?? []).map((b: { user: string; hash?: string }) => ({ user: b.user, password: "", hash: b.hash })),
    faUrl: o.forward_auth?.url ?? "",
    faHeaders: (o.forward_auth?.response_headers ?? []).join("\n"),
    strip: !!o.strip_prefix,
    rwFrom: o.rewrite?.from ?? "",
    rwTo: o.rewrite?.to ?? "",
    maint: o.maintenance != null,
    maintHtml: o.maintenance ?? MAINTENANCE,
    timeout: o.timeout_s == null ? "" : String(o.timeout_s),
    maxBody: o.max_body_mb == null ? "" : String(o.max_body_mb),
    errorPages: o.error_pages ?? {},
  };
}

function toBody(f: Form) {
  return {
    host: f.host.trim().toLowerCase(),
    path: f.path.trim() || "/",
    port: Number(f.port),
    container: f.container || undefined,
    https: f.https,
    via_tunnel: f.via_tunnel,
    options: {
      www_redirect: f.www || null,
      redirects: f.redirects.filter((r) => r.from && r.to),
      // Existing users keep their stored hash. A new or changed password travels in plaintext over
      // the panel connection, and the backend is expected to hash it (it takes hashes only today).
      basic_auth: f.basic
        .filter((b) => b.user && (b.password || b.hash))
        .map((b) => (b.password ? { user: b.user, password: b.password } : { user: b.user, hash: b.hash })),
      forward_auth: f.faUrl.trim() ? { url: f.faUrl.trim(), response_headers: lines(f.faHeaders) } : null,
      strip_prefix: f.strip,
      rewrite: f.rwFrom ? { from: f.rwFrom, to: f.rwTo } : null,
      maintenance: f.maint ? f.maintHtml || MAINTENANCE : null,
      error_pages: f.errorPages,
      timeout_s: num(f.timeout),
      max_body_mb: num(f.maxBody),
    },
  };
}

export function Domains({ c }: { c: Ctx }) {
  const canEdit = atLeast(c.d.role, "operator");
  const [edit, setEdit] = useState<{ id?: string; f: Form } | null>(null);
  const list = c.d.domains;
  return (
    <Card
      title="Domains"
      actions={
        canEdit && (
          <button class="btn btn-primary" onClick={() => setEdit({ f: toForm() })}>
            Add domain
          </button>
        )
      }
    >
      <p class="mb-4 text-dim">
        Point a domain's DNS records at this server, and Tugport routes its traffic to this service with an automatic Let's Encrypt certificate.
      </p>
      {list.length === 0 ? (
        <Empty>This service has no domains yet.</Empty>
      ) : (
        <ul class="divide-y divide-line rounded-md border border-line">
          {list.map((d) => {
            const o = d.options ?? {};
            const url = `${d.https ? "https" : "http"}://${d.host}${d.path}`;
            return (
              <li key={d.id} class="flex flex-wrap items-center gap-2 px-3 py-2.5">
                <div class="min-w-0 flex-1">
                  <a class="link break-all font-medium" href={url} target="_blank" rel="noreferrer">
                    {d.host}
                    {d.path === "/" ? "" : d.path}
                  </a>
                  <div class="mt-1 flex flex-wrap gap-1.5 text-xs">
                    <span class="chip">
                      port {d.port}
                      {d.container ? ` on ${d.container}` : ""}
                    </span>
                    <span class="chip">{d.https ? "HTTPS" : "HTTP only"}</span>
                    {d.via_tunnel && <span class="chip">Cloudflare Tunnel</span>}
                    {o.www_redirect && <span class="chip">{o.www_redirect === "to_www" ? "redirects to www" : "redirects to apex"}</span>}
                    {o.basic_auth?.length > 0 && <span class="chip">basic auth</span>}
                    {o.forward_auth && <span class="chip">forward auth</span>}
                    {o.maintenance != null && <span class="chip text-warn">maintenance page</span>}
                  </div>
                </div>
                {canEdit && (
                  <span class="flex gap-2">
                    <button class="btn" onClick={() => setEdit({ id: d.id, f: toForm(d) })}>
                      Edit
                    </button>
                    <Action class="btn btn-ghost text-bad" confirm={`Remove ${d.host}${d.path === "/" ? "" : d.path}?`} run={() => del(`/domains/${d.id}`).then(c.reload)}>
                      Remove
                    </Action>
                  </span>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {edit && <DomainForm c={c} id={edit.id} init={edit.f} onClose={() => setEdit(null)} />}
    </Card>
  );
}

function DomainForm(props: { c: Ctx; id?: string; init: Form; onClose: () => void }) {
  const [f, setF] = useState(props.init);
  const set = (p: Partial<Form>) => setF((s) => ({ ...s, ...p }));
  const text = (k: keyof Form) => ({ value: f[k] as string, onInput: (e: { currentTarget: { value: string } }) => set({ [k]: e.currentTarget.value }) });
  const s = useSubmit(async () => {
    if (props.id) await patch(`/domains/${props.id}`, toBody(f));
    else await post(`/services/${props.c.d.service.id}/domains`, toBody(f));
    await props.c.reload();
    props.onClose();
  });
  const composeServices = [...new Set(props.c.d.status.replicas.map((r) => r.compose_service).filter((x): x is string => !!x))];
  const isCompose = props.c.d.service.kind === "compose";
  const advanced = props.init.redirects.length > 0 || props.init.www !== "";
  const guarded = props.init.basic.length > 0 || props.init.faUrl !== "";
  const routing = props.init.strip || props.init.rwFrom !== "";
  const limits = props.init.timeout !== "" || props.init.maxBody !== "";

  return (
    <Modal open onClose={props.onClose} title={props.id ? `Edit ${props.init.host}` : "Add domain"} wide>
      <form class="space-y-4" onSubmit={s.submit}>
        <div class="grid gap-4 sm:grid-cols-[1fr_8rem_8rem]">
          <Field label="Domain">
            <input class="input font-mono" required autoFocus={!props.id} placeholder="app.example.com" spellcheck={false} autoComplete="off" {...text("host")} />
          </Field>
          <Field label="Path">
            <input class="input font-mono" required pattern="/.*" {...text("path")} />
          </Field>
          <Field label="Container port">
            <input class="input font-mono" required type="number" min={1} max={65535} placeholder="3000" {...text("port")} />
          </Field>
        </div>
        {isCompose && (
          <Field label="Compose service" hint="Pick the container in the stack that receives this domain's traffic.">
            <Select value={f.container} onChange={(container) => set({ container })} options={[["", "Choose…"], ...composeServices.map((n): [string, string] => [n, n])]} />
          </Field>
        )}
        <Check checked={f.https} onChange={(https) => set({ https })} label="HTTPS" hint="The domain gets a Let's Encrypt certificate, and plain HTTP redirects to HTTPS." />
        <Check
          checked={f.via_tunnel}
          onChange={(via_tunnel) => set({ via_tunnel })}
          label="Through the Cloudflare Tunnel"
          hint="Traffic for this domain arrives through the tunnel from Settings instead of the server's public address."
        />

        <details open={advanced} class="rounded-md border border-line px-3 py-2">
          <summary class="font-medium">Redirects</summary>
          <div class="mt-3 space-y-3">
            <Field label="www">
              <Select
                value={f.www}
                onChange={(www) => set({ www })}
                options={[
                  ["", "No www redirect"],
                  ["to_www", "Redirect the bare domain to www"],
                  ["to_apex", "Redirect www to the bare domain"],
                ]}
              />
            </Field>
            {f.redirects.map((r, i) => (
              <div key={i} class="grid items-end gap-2 sm:grid-cols-[1fr_1fr_auto_auto]">
                <Field label="From (regular expression)">
                  <input
                    class="input font-mono"
                    value={r.from}
                    placeholder="^https://old\.example\.com/(.*)"
                    onInput={(e) => set({ redirects: f.redirects.map((x, j) => (j === i ? { ...x, from: e.currentTarget.value } : x)) })}
                  />
                </Field>
                <Field label="To">
                  <input
                    class="input font-mono"
                    value={r.to}
                    placeholder="https://example.com/$1"
                    onInput={(e) => set({ redirects: f.redirects.map((x, j) => (j === i ? { ...x, to: e.currentTarget.value } : x)) })}
                  />
                </Field>
                <label class="flex items-center gap-2 pb-2">
                  <input
                    type="checkbox"
                    checked={r.permanent}
                    onChange={(e) => set({ redirects: f.redirects.map((x, j) => (j === i ? { ...x, permanent: e.currentTarget.checked } : x)) })}
                  />
                  Permanent
                </label>
                <button type="button" class="btn btn-ghost text-bad" onClick={() => set({ redirects: f.redirects.filter((_, j) => j !== i) })}>
                  Remove
                </button>
              </div>
            ))}
            <button type="button" class="btn" onClick={() => set({ redirects: [...f.redirects, { from: "", to: "", permanent: false }] })}>
              Add redirect
            </button>
          </div>
        </details>

        <details open={guarded} class="rounded-md border border-line px-3 py-2">
          <summary class="font-medium">Access control</summary>
          <div class="mt-3 space-y-3">
            <p class="text-xs text-muted">Basic auth asks visitors for a user name and password. Tugport stores only a hash of each password.</p>
            {f.basic.map((b, i) => (
              <div key={i} class="grid items-end gap-2 sm:grid-cols-[1fr_1fr_auto]">
                <Field label="User">
                  <input
                    class="input"
                    autoComplete="off"
                    value={b.user}
                    onInput={(e) => set({ basic: f.basic.map((x, j) => (j === i ? { ...x, user: e.currentTarget.value } : x)) })}
                  />
                </Field>
                <Field label="Password">
                  <input
                    class="input"
                    type="password"
                    autoComplete="new-password"
                    placeholder={b.hash ? "Unchanged" : ""}
                    value={b.password}
                    onInput={(e) => set({ basic: f.basic.map((x, j) => (j === i ? { ...x, password: e.currentTarget.value } : x)) })}
                  />
                </Field>
                <button type="button" class="btn btn-ghost text-bad" onClick={() => set({ basic: f.basic.filter((_, j) => j !== i) })}>
                  Remove
                </button>
              </div>
            ))}
            <button type="button" class="btn" onClick={() => set({ basic: [...f.basic, { user: "", password: "" }] })}>
              Add basic auth user
            </button>
            <Field label="Forward auth URL" hint="Every request is first sent to this URL, and only a 2xx answer lets it through.">
              <input class="input font-mono" type="url" placeholder="http://auth:4181/verify" {...text("faUrl")} />
            </Field>
            {f.faUrl && (
              <Field label="Headers to copy from the auth response" hint="Write one header per line, like X-Forwarded-User.">
                <textarea class="input h-20" spellcheck={false} {...text("faHeaders")} />
              </Field>
            )}
          </div>
        </details>

        <details open={routing} class="rounded-md border border-line px-3 py-2">
          <summary class="font-medium">Path handling</summary>
          <div class="mt-3 space-y-3">
            <Check checked={f.strip} onChange={(strip) => set({ strip })} label="Strip the path prefix" hint={`The app sees /users instead of ${f.path.replace(/\/$/, "") || ""}/users.`} />
            <div class="grid gap-2 sm:grid-cols-2">
              <Field label="Rewrite from (regular expression)">
                <input class="input font-mono" placeholder="^/api/v1/(.*)" {...text("rwFrom")} />
              </Field>
              <Field label="Rewrite to">
                <input class="input font-mono" placeholder="/v1/$1" {...text("rwTo")} />
              </Field>
            </div>
          </div>
        </details>

        <details open={f.maint} class="rounded-md border border-line px-3 py-2">
          <summary class="font-medium">Maintenance page</summary>
          <div class="mt-3 space-y-3">
            <Check checked={f.maint} onChange={(maint) => set({ maint })} label="Show the maintenance page" hint="Every request gets this page with status 503 until you turn it off." />
            {f.maint && (
              <Field label="Page HTML">
                <textarea class="input h-40" spellcheck={false} {...text("maintHtml")} />
              </Field>
            )}
          </div>
        </details>

        <details open={limits} class="rounded-md border border-line px-3 py-2">
          <summary class="font-medium">Limits</summary>
          <div class="mt-3 grid gap-3 sm:grid-cols-2">
            <Field label="Timeout in seconds" hint="Leave this empty for the default.">
              <input class="input" type="number" min={1} {...text("timeout")} />
            </Field>
            <Field label="Largest request body in MB" hint="Leave this empty for the default.">
              <input class="input" type="number" min={1} {...text("maxBody")} />
            </Field>
          </div>
        </details>

        <ErrorBox error={s.error} />
        <div class="flex justify-end gap-2">
          <button type="button" class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={s.busy}>
            {props.id ? "Save domain" : "Add domain"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
