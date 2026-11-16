import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";
import { atLeast, del, patch, post, useApi, useMe, type Certificate, type Domain } from "../api";
import { Icon } from "../icons";
import { Action, Card, Check, Empty, Field, FormButtons, IconButton, Input, Modal, Note, Select, TextArea, useForm, useSubmit } from "../ui";
import { ago, lines, num, patchAt, qs, removeAt, str } from "../util";
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
  /** `saved` is the user's stored name; sent without a password, that user keeps the current one. */
  basic: { user: string; password: string; saved?: string }[];
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
    basic: (o.basic_auth ?? []).map((b: { user: string }) => ({ user: b.user, password: "", saved: b.user })),
    faUrl: o.forward_auth?.url ?? "",
    faHeaders: (o.forward_auth?.response_headers ?? []).join("\n"),
    strip: !!o.strip_prefix,
    rwFrom: o.rewrite?.from ?? "",
    rwTo: o.rewrite?.to ?? "",
    maint: o.maintenance != null,
    maintHtml: o.maintenance ?? MAINTENANCE,
    timeout: str(o.timeout_s),
    maxBody: str(o.max_body_mb),
    errorPages: o.error_pages ?? {},
  };
}

function toBody(f: Form) {
  return {
    host: f.host.trim().toLowerCase(),
    path: f.path.trim() || "/",
    port: Number(f.port),
    container: f.container || null,
    https: f.https,
    via_tunnel: f.via_tunnel,
    options: {
      www_redirect: f.www || null,
      redirects: f.redirects.filter((r) => r.from && r.to),
      // The server hashes new passwords; a user sent without one keeps the stored password.
      basic_auth: f.basic.filter((b) => b.user).map((b) => (b.password ? { user: b.user, password: b.password } : { user: b.user })),
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
  // The image's EXPOSE ports, from its last deploy, suggest the port of a new domain.
  const port = String(c.d.exposed_ports?.[0] ?? "");
  return (
    <Card
      title="Domains"
      actions={
        canEdit && (
          <button class="btn btn-primary" onClick={() => setEdit({ f: { ...toForm(), port } })}>
            Add domain
          </button>
        )
      }
    >
      <p class="mb-4 text-dim">
        {(c.nodes?.nodes.length ?? 0) > 1
          ? "Point a domain's DNS records at any of your servers, and Tugport routes its traffic to this service wherever it runs, with an automatic Let's Encrypt certificate."
          : "Point a domain's DNS records at this server, and Tugport routes its traffic to this service with an automatic Let's Encrypt certificate."}
      </p>
      {list.length === 0 ? (
        <Empty>This service has no domains yet.</Empty>
      ) : (
        <ul class="inset-list">
          {list.map((d) => {
            const o = d.options ?? {};
            const url = `${d.https ? "https" : "http"}://${d.host}${d.path}`;
            return (
              <li key={d.id} class="flex flex-wrap items-center gap-2 px-3 py-2.5">
                <div class="min-w-0 flex-1">
                  {d.host.startsWith("*.") ? (
                    <span class="break-all font-medium">
                      {d.host}
                      {d.path === "/" ? "" : d.path}
                      <span class="ml-2 text-xs font-normal text-muted">every name one level under {d.host.slice(2)}</span>
                    </span>
                  ) : (
                    <a class="link break-all font-medium" href={url} target="_blank" rel="noreferrer">
                      {d.host}
                      {d.path === "/" ? "" : d.path}
                    </a>
                  )}
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
                    {d.https && !d.via_tunnel && <CertChip c={d.certificate} />}
                  </div>
                  {d.https && !d.via_tunnel && d.certificate?.last_error && (
                    <p class="mt-1.5 break-words text-xs text-bad">{certError(d.certificate)}</p>
                  )}
                </div>
                {canEdit && (
                  <span class="flex gap-2">
                    <IconButton icon="edit" label={`Edit ${d.host}`} onClick={() => setEdit({ id: d.id, f: toForm(d) })} />
                    <Action
                      class="btn btn-ghost btn-sm px-2 text-bad"
                      label={`Remove ${d.host}`}
                      confirm={`Remove ${d.host}${d.path === "/" ? "" : d.path}?`}
                      run={() => del(`/domains/${d.id}`).then(c.reload)}
                    >
                      <Icon name="trash" />
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

/** A failed certificate's reason, as a sentence, and when Tugport tries again. */
export function certError(c: Certificate): string {
  const reason = /[.!?]$/.test(c.last_error ?? "") ? c.last_error : `${c.last_error}.`;
  return c.next_attempt_at ? `${reason} Tugport tries again ${ago(c.next_attempt_at)}.` : (reason ?? "");
}

/** Where a domain's certificate stands: issued (and how it was proven), failing or on its way. */
export function CertChip(props: { c?: Certificate | null }) {
  const c = props.c;
  if (c?.issued) {
    const how = c.source === "acme-dns" ? " via DNS" : c.source === "imported" ? " (imported)" : "";
    const soon = c.not_after != null && c.not_after - Date.now() / 1000 < 14 * 86400;
    return <span class={`chip ${soon ? "text-warn" : ""}`}>{`certificate${how}, expires ${ago(c.not_after)}`}</span>;
  }
  if (c?.last_error) return <span class="chip text-bad">certificate failed</span>;
  return <span class="chip text-warn">certificate pending</span>;
}

interface HostRoute {
  domain_id: string;
  host: string;
  path: string;
  service_id: string | null;
  service: string | null;
  project: string | null;
}

/**
 * What the typed name means before it is saved: what a wildcard catches, whether its certificate
 * can be issued, and which other routes share the name (the longest path wins).
 */
function HostInfo(props: { id?: string; host: string; path: string; https: boolean; tunnel: boolean }) {
  const owner = useMe().principal.is_owner;
  const [host, setHost] = useState(props.host.trim().toLowerCase());
  useEffect(() => {
    const t = setTimeout(() => setHost(props.host.trim().toLowerCase()), 300);
    return () => clearTimeout(t);
  }, [props.host]);
  const r = useApi<{ routes: HostRoute[]; dns_certificates: boolean }>(host.includes(".") ? `/hosts${qs({ host })}` : null);
  const wildcard = host.startsWith("*.");
  const base = host.slice(2);
  const others = (r.data?.routes ?? []).filter((x) => x.domain_id !== props.id);
  const same = others.filter((x) => x.host === host);
  const covering = others.filter((x) => x.host !== host && x.host.startsWith("*."));
  const under = others.filter((x) => x.host !== host && !x.host.startsWith("*."));
  const path = props.path.trim() || "/";
  const target = (x: HostRoute) =>
    x.service ? (
      <a class="link" href={`/projects/${x.project}/${x.service}/domains`}>
        {x.service}
        <span class="text-muted"> in {x.project}</span>
      </a>
    ) : (
      <span class="text-muted">a service in another project</span>
    );
  const route = (x: HostRoute) => (
    <li key={x.domain_id} class="flex flex-wrap gap-x-2">
      <span class="font-mono">
        {x.host}
        {x.path}
      </span>
      <span class="text-muted">→</span>
      {target(x)}
    </li>
  );
  return (
    <>
      {wildcard && (
        <Note>
          This catches every name one level under <span class="font-mono">{base}</span>, like <span class="font-mono">shop.{base}</span>, that has no domain of its own.
          Names you add separately, here or in other services, keep their own routes.
        </Note>
      )}
      {wildcard && props.tunnel && <Note tone="bad">Wildcards can't go through the Cloudflare Tunnel; add each name instead.</Note>}
      {wildcard && props.https && r.data && !r.data.dns_certificates && (
        <Note tone="warn">
          A wildcard certificate is proven with a DNS record, so it needs a Cloudflare API token.{" "}
          {owner ? (
            <a class="link" href="/settings/certificates">
              Add one in Settings → Certificates
            </a>
          ) : (
            "Ask the owner to add one in Settings → Certificates"
          )}
          , or turn HTTPS off.
        </Note>
      )}
      {same.length > 0 && (
        <Note tone={same.some((x) => x.path === path) ? "bad" : "info"}>
          <p class="mb-1">
            {same.some((x) => x.path === path) ? `${host}${path} is already taken. ` : ""}
            Other routes on this name:
          </p>
          <ul class="space-y-0.5">{same.map(route)}</ul>
          <p class="mt-1 text-xs text-muted">
            Each request goes to the route with the longest path that matches it, WebSocket upgrades included. The app sees the full path unless you strip the prefix
            below.
          </p>
        </Note>
      )}
      {covering.length > 0 && (
        <Note>
          This name takes over from the wildcard: <ul class="mt-1 space-y-0.5">{covering.map(route)}</ul>
        </Note>
      )}
      {wildcard && under.length > 0 && (
        <Note>
          These names have their own routes and stay with them: <ul class="mt-1 space-y-0.5">{under.map(route)}</ul>
        </Note>
      )}
    </>
  );
}

/** A collapsible group of advanced options, open when it holds settings. */
function Fold(props: { title: string; open: boolean; class?: string; children: ComponentChildren }) {
  return (
    <details open={props.open} class="rounded-md border border-line px-3 py-2">
      <summary class="font-medium">{props.title}</summary>
      <div class={`mt-3 ${props.class ?? "space-y-3"}`}>{props.children}</div>
    </details>
  );
}

function DomainForm(props: { c: Ctx; id?: string; init: Form; onClose: () => void }) {
  const { f, set, bind } = useForm(props.init);
  const redirect = (i: number, p: Partial<Form["redirects"][number]>) => set({ redirects: patchAt(f.redirects, i, p) });
  const basic = (i: number, p: Partial<Form["basic"][number]>) => set({ basic: patchAt(f.basic, i, p) });
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
          <Input label="Domain" mono required autoFocus={!props.id} placeholder="app.example.com or *.example.com" autoComplete="off" {...bind("host")} />
          <Input
            label="Path"
            info="Requests under this path come to this service. Other services, even in other projects, can take other paths on the same name; the longest matching path wins."
            class="font-mono"
            required
            pattern="/.*"
            {...bind("path")}
          />
          <Input label="Container port" class="font-mono" required type="number" min={1} max={65535} placeholder="3000" {...bind("port")} />
          {(props.c.d.exposed_ports?.length ?? 0) > 0 && (
            <p class="-mt-2 text-xs text-muted sm:col-span-3">
              The image exposes {props.c.d.exposed_ports?.length === 1 ? "port" : "ports"} {props.c.d.exposed_ports?.join(", ")}.
            </p>
          )}
        </div>
        <HostInfo id={props.id} host={f.host} path={f.path} https={f.https} tunnel={f.via_tunnel} />
        {isCompose && (
          <Field label="Compose service" hint="Pick the container in the stack that receives this domain's traffic.">
            <Select value={f.container} onChange={(container) => set({ container })} options={[["", "Choose…"], ...composeServices.map((n): [string, string] => [n, n])]} />
          </Field>
        )}
        <Check
          checked={f.https}
          onChange={(https) => set({ https })}
          label="HTTPS"
          hint="The domain gets a Let's Encrypt certificate, and plain HTTP redirects to HTTPS. With a Cloudflare token in Settings → Certificates, names in your Cloudflare zones are proven over DNS, so port 80 can stay closed."
        />
        <Check
          checked={f.via_tunnel}
          onChange={(via_tunnel) => set({ via_tunnel })}
          label="Through the Cloudflare Tunnel"
          hint="Traffic for this domain arrives through the tunnel from Settings instead of the server's public address."
        />

        <Fold title="Redirects" open={advanced}>
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
              <Input
                label="From (regular expression)"
                class="font-mono"
                value={r.from}
                placeholder="^https://old\.example\.com/(.*)"
                onInput={(e) => redirect(i, { from: e.currentTarget.value })}
              />
              <Input label="To" class="font-mono" value={r.to} placeholder="https://example.com/$1" onInput={(e) => redirect(i, { to: e.currentTarget.value })} />
              <label class="flex items-center gap-2 pb-2">
                <input type="checkbox" checked={r.permanent} onChange={(e) => redirect(i, { permanent: e.currentTarget.checked })} />
                Permanent
              </label>
              <IconButton icon="trash" label="Remove this redirect" class="h-11 text-bad md:h-9" onClick={() => set({ redirects: removeAt(f.redirects, i) })} />
            </div>
          ))}
          <button type="button" class="btn" onClick={() => set({ redirects: [...f.redirects, { from: "", to: "", permanent: false }] })}>
            Add redirect
          </button>
        </Fold>

        <Fold title="Access control" open={guarded}>
          <p class="text-xs text-muted">Basic auth asks visitors for a user name and password. Tugport stores only a hash of each password.</p>
          {f.basic.map((b, i) => (
            <div key={i} class="grid items-end gap-2 sm:grid-cols-[1fr_1fr_auto]">
              <Input label="User" autoComplete="off" value={b.user} onInput={(e) => basic(i, { user: e.currentTarget.value })} />
              <Input
                label="Password"
                type="password"
                autoComplete="new-password"
                required={!!b.user && b.user !== b.saved}
                placeholder={b.user && b.user === b.saved ? "Unchanged" : ""}
                value={b.password}
                onInput={(e) => basic(i, { password: e.currentTarget.value })}
              />
              <IconButton icon="trash" label="Remove this login" class="h-11 text-bad md:h-9" onClick={() => set({ basic: removeAt(f.basic, i) })} />
            </div>
          ))}
          <button type="button" class="btn" onClick={() => set({ basic: [...f.basic, { user: "", password: "" }] })}>
            Add basic auth user
          </button>
          <Input
            label="Forward auth URL"
            hint="Every request is first sent to this URL, and only a 2xx answer lets it through."
            class="font-mono"
            type="url"
            placeholder="http://auth:4181/verify"
            {...bind("faUrl")}
          />
          {f.faUrl && <TextArea label="Headers to copy from the auth response" hint="Write one header per line, like X-Forwarded-User." class="h-20" {...bind("faHeaders")} />}
        </Fold>

        <Fold title="Path handling" open={routing}>
          <Check
            checked={f.strip}
            onChange={(strip) => set({ strip })}
            label="Strip the path prefix"
            hint={`The app sees /users instead of ${f.path.replace(/\/$/, "") || ""}/users.`}
          />
          <div class="grid gap-2 sm:grid-cols-2">
            <Input label="Rewrite from (regular expression)" class="font-mono" placeholder="^/api/v1/(.*)" {...bind("rwFrom")} />
            <Input label="Rewrite to" class="font-mono" placeholder="/v1/$1" {...bind("rwTo")} />
          </div>
        </Fold>

        <Fold title="Maintenance page" open={f.maint}>
          <Check
            checked={f.maint}
            onChange={(maint) => set({ maint })}
            label="Show the maintenance page"
            hint="Every request gets this page with status 503 until you turn it off."
          />
          {f.maint && <TextArea label="Page HTML" class="h-40" {...bind("maintHtml")} />}
        </Fold>

        <Fold title="Limits" open={limits} class="grid gap-3 sm:grid-cols-2">
          <Input label="Timeout in seconds" hint="Leave this empty for the default." type="number" min={1} {...bind("timeout")} />
          <Input label="Largest request body in MB" hint="Leave this empty for the default." type="number" min={1} {...bind("maxBody")} />
        </Fold>

        <FormButtons s={s} submit={props.id ? "Save domain" : "Add domain"} onCancel={props.onClose} />
      </form>
    </Modal>
  );
}
