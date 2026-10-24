import type { ComponentChildren } from "preact";
import { useCallback, useEffect, useRef, useState } from "preact/hooks";
import { FINAL, atLeast, del, get, hasGrant, nodeName, post, put, useApi, useEvent, useMe, useNodes, type Deploy, type Node, type Replica, type ServiceDetail } from "../api";
import { EnvEditor } from "../env-editor";
import { Icon } from "../icons";
import { route } from "../router";
import {
  Action,
  Card,
  CardSkeleton,
  Check,
  CodeBlock,
  ConfirmName,
  Empty,
  ErrorBox,
  FormButtons,
  IconButton,
  Input,
  Loaded,
  LoadingPage,
  Modal,
  Note,
  Page,
  Pill,
  Rows,
  RowsSkeleton,
  Skel,
  Tabs,
  TextArea,
  errText,
  toast,
  useSubmit,
} from "../ui";
import { fmt } from "../charts";
import { ago, bytes, clock, duration, qs, sumUsage, varName } from "../util";
import { ReadReplicas, ReplicaOf } from "./database";
import { Domains } from "./domains";
import { DeployKey } from "./new-service";
import { Console, Logs, ServiceMetrics, tailLines, useStickToBottom, type Now } from "./observe";
import { sourceLabel } from "./projects";
import { ServiceSettings } from "./service-settings";

const TABS: [string, string][] = [
  ["overview", "Overview"],
  ["deploys", "Deploys"],
  ["logs", "Logs"],
  ["metrics", "Metrics"],
  ["replicas", "Read replicas"],
  ["domains", "Domains"],
  ["env", "Environment"],
  ["files", "Files"],
  ["settings", "Settings"],
  ["console", "Console"],
  ["danger", "Danger zone"],
];
const LIVE = new Set(["deploy", "service.status", "service.updated"]);

export interface Ctx {
  d: ServiceDetail;
  reload: () => Promise<void>;
  openLog: (deployId: string) => void;
  /** The servers, when the signed-in user may list them. */
  nodes: { mesh: boolean; nodes: Node[] } | null;
}

/** Which server runs a replica matters once there is more than one. */
const multiServer = (c: Ctx) => (c.nodes?.nodes.length ?? 0) > 1 || c.d.status.replicas.some((r) => r.node !== "local");

/** Starts a deploy and opens its live log. */
export async function deploy(c: Ctx) {
  const r = await post<{ deploy_id: string }>(`/services/${c.d.service.id}/deploy`, {});
  c.openLog(r.deploy_id);
}

/** Rolls back to a deploy (the previous image without one) and opens the rollback's log. */
async function rollback(c: Ctx, deployId?: string) {
  const r = await post<{ deploy_id: string }>(`/services/${c.d.service.id}/rollback`, deployId ? { deploy_id: deployId } : {});
  c.openLog(r.deploy_id);
}

/** "push by alice · 5 minutes ago" */
const started = (d: Deploy) => `${d.trigger}${d.actor ? ` by ${d.actor}` : ""} · ${ago(d.created_at)}`;

/** "Saved" note with a button that applies the change through a new deploy. */
export function RedeployNote(props: { c: Ctx; children: ComponentChildren }) {
  return (
    <span class="flex flex-wrap items-center justify-between gap-2">
      <span>{props.children}</span>
      {atLeast(props.c.d.role, "deployer") && (
        <Action class="btn btn-primary" run={() => deploy(props.c)}>
          Redeploy now
        </Action>
      )}
    </span>
  );
}

export function ServicePage(props: { project?: string; service?: string; tab?: string }) {
  const me = useMe();
  const sid = useRef("");
  // Replicas on a server that drops out vanish from the status without a container event.
  const r = useApi<ServiceDetail>(`/services/${props.project}.${props.service}`, (e) => (e.service_id === sid.current && LIVE.has(e.kind)) || e.kind === "nodes");
  sid.current = r.data?.service.id ?? "";
  const nodes = useNodes();
  const [logId, setLogId] = useState<string | null>(null);
  const d = r.data;
  if (!d) return <LoadingPage title={props.service} crumbs={[["Projects", "/"], [props.project ?? "", `/projects/${props.project}`]]} r={r} skeleton={<ServiceSkeleton />} />;
  const s = d.service;
  // Read replicas belong to primary databases; a replica follows one instead.
  const tabs = TABS.filter(([id]) => id !== "replicas" || (s.kind === "database" && !s.spec.database?.primary));
  const tab = tabs.some(([id]) => id === props.tab) ? (props.tab as string) : "overview";
  const base = `/projects/${s.project}/${s.name}`;
  const c: Ctx = { d, reload: r.reload, openLog: setLogId, nodes };
  const label = (x: Replica) => (x.compose_service ?? x.name) + (multiServer(c) ? ` on ${nodeName(nodes, x.node)}` : "");
  const site = d.domains[0];
  return (
    <Page
      title={s.name}
      crumbs={[
        ["Projects", "/"],
        [s.project, `/projects/${s.project}`],
      ]}
      meta={
        <>
          <Pill state={d.status.state} />
          <span class="chip">{s.kind}</span>
        </>
      }
      sub={
        site && (
          <a class="inline-flex items-center gap-1.5 text-dim hover:text-text" href={`${site.https ? "https" : "http"}://${site.host}${site.path}`} target="_blank" rel="noreferrer">
            {site.host}
            {site.path === "/" ? "" : site.path}
            <Icon name="external" class="size-3.5" />
          </a>
        )
      }
      actions={
        atLeast(d.role, "deployer") && (
          <Action class="btn btn-primary" run={() => deploy(c)}>
            {d.status.live_deploy ? "Redeploy" : "Deploy"}
          </Action>
        )
      }
    >
      <Tabs label="Service sections" current={tab} items={tabs.map(([id, label]) => [id, label, id === "overview" ? base : `${base}/${id}`])} />
      {tab === "overview" && <Overview c={c} />}
      {tab === "deploys" && <Deploys c={c} />}
      {tab === "replicas" && <ReadReplicas c={c} />}
      {tab === "logs" && <Logs service={s} replicas={d.status.replicas} label={label} />}
      {tab === "metrics" && <ServiceMetrics service={s} />}
      {tab === "domains" && <Domains c={c} />}
      {tab === "env" && <Env c={c} />}
      {tab === "files" && <Files c={c} />}
      {tab === "settings" && <ServiceSettings c={c} />}
      {tab === "console" && <Console service={s} replicas={d.status.replicas} label={label} canUse={atLeast(d.role, "operator") && hasGrant(me, "secrets:read")} />}
      {tab === "danger" && <Danger c={c} />}
      <DeployLog id={logId} onClose={() => setLogId(null)} />
    </Page>
  );
}

function Overview({ c }: { c: Ctx }) {
  const { d } = c;
  const s = d.service;
  const st = d.status;
  const canDeploy = atLeast(d.role, "deployer");
  const id = s.id;
  const src = s.spec.source;
  const envName = varName(s.name, "URL");
  const primaryId = s.spec.database?.primary;
  return (
    <div class="grid gap-6 lg:grid-cols-3">
      <div class="min-w-0 space-y-6 lg:col-span-2">
        <Card
          title="Status"
          actions={
            canDeploy && (
              <>
                {st.live_deploy && (
                  <Action confirm="Roll back to the previous image?" run={() => rollback(c)}>
                    Roll back
                  </Action>
                )}
                <Action run={() => post(`/services/${id}/restart`).then(c.reload)} disabled={st.running === 0} done="Restarted.">
                  Restart
                </Action>
                {st.running > 0 ? (
                  <Action
                    class="btn btn-danger"
                    confirm={`Stop ${s.name}? Its containers stop serving until you start them again.`}
                    run={() => post(`/services/${id}/stop`).then(c.reload)}
                    done="Stopped."
                  >
                    Stop
                  </Action>
                ) : (
                  <Action run={() => post(`/services/${id}/start`).then(c.reload)} disabled={st.state === "not deployed"} done="Started.">
                    Start
                  </Action>
                )}
              </>
            )
          }
        >
          <p class="mb-4 flex flex-wrap items-center gap-3">
            <Pill state={st.state} />
            <span class="text-dim">
              {st.running} of {st.desired} {st.desired === 1 ? "replica" : "replicas"} running
            </span>
          </p>
          {st.replicas.length === 0 ? <p class="text-muted">There are no containers yet, so deploy the service to start it.</p> : <Replicas c={c} />}
          {st.running > 0 && <Usage id={id} nodes={st.replicas.filter((x) => x.state === "running").map((x) => x.node)} />}
          <p class="mt-4 text-xs text-muted">
            Other services in this project reach it by the host name <code>{s.name}</code>
            {s.kind === "database" && (
              <>
                , and they can connect through the project variable <code>{envName}</code>
              </>
            )}
            .
          </p>
        </Card>
        <LastDeploy c={c} />
      </div>
      <div class="min-w-0 space-y-6">
        {primaryId && <ReplicaOf c={c} primaryId={primaryId} />}
        <Card title="Source">
          <p class="break-all font-mono text-[13px]">{sourceLabel(src, s.kind)}</p>
          {src.type === "git" && (
            <ul class="mt-2 space-y-1 text-dim">
              <li>From {src.provider ? "a connected Git host" : "a URL"}</li>
              <li>Build: {s.spec.build?.type ?? "auto"}</li>
              <li>{src.auto_deploy ? "Deploys on every push" : "Deploys only when started by hand or by webhook"}</li>
              {src.watch_paths.length > 0 && <li>Watches {src.watch_paths.join(", ")}</li>}
            </ul>
          )}
          {s.spec.auto_update?.enabled && src.type === "image" && <p class="mt-2 text-dim">Checks for a new image every {s.spec.auto_update.interval_minutes} minutes.</p>}
          <a class="link mt-3 inline-block text-sm" href={`/projects/${s.project}/${s.name}/settings`}>
            Change source and build
          </a>
        </Card>
        {s.kind !== "database" && <Webhook c={c} />}
        {src.type === "git" && !src.provider && atLeast(d.role, "operator") && (
          <Card title="Deploy key">
            <p class="mb-3 text-dim">A private repository reached by an SSH URL needs a deploy key that Tugport generates for this service.</p>
            <DeployKey serviceId={id} />
          </Card>
        )}
      </div>
    </div>
  );
}

/**
 * Current CPU and memory, added up over the servers its replicas run on. The panel's own sample
 * always joins in, because it counts the crashes on every server.
 */
function Usage(props: { id: string; nodes: string[] }) {
  const servers = [...new Set(props.nodes)];
  const key = [...new Set(["local", ...servers])].sort().join(",");
  const [u, setU] = useState<ReturnType<typeof sumUsage>>(null);
  const load = useCallback(async () => {
    const samples = await Promise.all(key.split(",").map((n) => get<Now>(`/metrics/now${qs({ node: n === "local" ? null : n })}`).catch(() => null)));
    setU(sumUsage(samples, props.id));
  }, [key, props.id]);
  useEffect(() => {
    load();
  }, [load]);
  // Only the owner gets "metrics" events; everyone else sees the numbers from when the page opened.
  useEvent((e) => (e.kind === "metrics" || e.kind === "resync") && load());
  if (!u) return null;
  // Docker reports 0 B where the memory controller is unavailable; leave it out rather than mislead.
  const memory = u.memory > 0 ? ` and ${bytes(u.memory)} of memory` : "";
  const across = servers.length > 1 ? ` across ${servers.length} servers` : "";
  const restarts = u.restarts ? `, after ${u.restarts} ${u.restarts === 1 ? "restart" : "restarts"}` : "";
  return (
    <p class="mt-3 text-dim">
      Using {fmt.pct(u.cpu)} of a core{memory}
      {across}
      {restarts}.
    </p>
  );
}

function Replicas({ c }: { c: Ctx }) {
  const st = c.d.status;
  const multi = multiServer(c);
  return (
    <ul class="inset-list">
      {st.replicas.map((r) => (
        <li key={r.id} class="flex flex-wrap items-center gap-x-4 gap-y-1 px-3 py-2">
          <span class="min-w-0 flex-1 truncate font-mono text-[13px]" title={r.name}>
            {r.compose_service ?? r.name}
          </span>
          {multi &&
            (c.nodes ? (
              <a class="chip hover:text-text" href={`/servers/${r.node}`} title="The server this replica runs on">
                on {nodeName(c.nodes, r.node)}
              </a>
            ) : (
              <span class="chip">on {nodeName(null, r.node)}</span>
            ))}
          <Pill state={r.health && r.state === "running" ? r.health : r.state} />
          {st.live_deploy && r.deploy !== st.live_deploy && <span class="chip">previous deploy</span>}
          <span class="w-full break-all text-xs text-muted">{[r.ip, shortImage(r.image), r.created ? `started ${ago(r.created)}` : ""].filter(Boolean).join(" · ")}</span>
        </li>
      ))}
    </ul>
  );
}

/** An image by digest is long and says little; its first 12 characters identify it. */
const shortImage = (image: string) => image.replace(/^(sha256:[0-9a-f]{12})[0-9a-f]+$/, "$1");

/** The overview's shape (tabs, the status card and the cards beside it) while it loads. */
const ServiceSkeleton = () => (
  <div role="status" aria-label="Loading">
    <div class="mb-7 flex gap-5 border-b border-line pb-3" aria-hidden="true">
      {["w-16", "w-14", "w-10", "w-14", "w-16", "w-20"].map((w, i) => (
        <Skel key={i} class={`h-4 ${w}`} />
      ))}
    </div>
    <div class="grid gap-6 lg:grid-cols-3">
      <div class="space-y-6 lg:col-span-2">
        <CardSkeleton>
          <div class="space-y-4 p-4 md:p-5">
            <Skel class="h-5 w-40" />
            <Skel class="h-14 w-full rounded-lg" />
            <Skel class="h-4 w-2/3" />
          </div>
        </CardSkeleton>
        <CardSkeleton />
      </div>
      <div class="space-y-6">
        <CardSkeleton />
        <CardSkeleton />
      </div>
    </div>
  </div>
);

function LastDeploy({ c }: { c: Ctx }) {
  const last = c.d.last_deploy;
  return (
    <Card
      title="Last deploy"
      actions={
        <a class="btn" href={`/projects/${c.d.service.project}/${c.d.service.name}/deploys`}>
          All deploys
        </a>
      }
    >
      {last ? <DeployRow d={last} live={c.d.status.live_deploy} c={c} /> : <p class="text-muted">This service has not been deployed yet.</p>}
    </Card>
  );
}

function DeployRow(props: { d: Deploy; live: string | null; c: Ctx; actions?: boolean }) {
  const { d, c } = props;
  const took = d.started_at && d.finished_at ? duration(d.finished_at - d.started_at) : "";
  const canDeploy = atLeast(c.d.role, "deployer");
  return (
    <div class="flex flex-wrap items-start gap-3">
      <div class="min-w-0 flex-1 space-y-1">
        <div class="flex flex-wrap items-center gap-2">
          <Pill state={d.status} />
          {d.id === props.live && <span class="chip">serving traffic</span>}
          <span class="text-dim">
            {started(d)}
            {took && ` · took ${took}`}
          </span>
        </div>
        {(d.commit_sha || d.ref) && (
          <p class="truncate text-dim">
            {d.commit_sha && <code>{d.commit_sha.slice(0, 7)}</code>} {d.commit_message?.split("\n")[0] ?? d.ref}
          </p>
        )}
        {d.image && <p class="truncate font-mono text-xs text-muted">{d.image}</p>}
        {d.error && <p class="text-bad">{d.error}</p>}
      </div>
      <div class="flex flex-wrap gap-2">
        <button class="btn" onClick={() => c.openLog(d.id)}>
          Log
        </button>
        {props.actions && canDeploy && !FINAL.has(d.status) && (
          <Action class="btn btn-danger" run={() => post(`/deploys/${d.id}/cancel`)} done="Cancelling.">
            Cancel
          </Action>
        )}
        {props.actions && canDeploy && d.status === "live" && d.id !== props.live && d.image && (
          <Action confirm={`Roll back to ${d.image}?`} run={() => rollback(c, d.id)}>
            Roll back to this
          </Action>
        )}
      </div>
    </div>
  );
}

function Webhook({ c }: { c: Ctx }) {
  const me = useMe();
  const canOperate = atLeast(c.d.role, "operator");
  // The URL deploys the service, so reading it back needs secrets:read; rotating shows a new one to any operator.
  const canRead = canOperate && hasGrant(me, "secrets:read");
  const hook = useApi<{ url: string }>(canRead ? `/services/${c.d.service.id}/hook` : null);
  const url = hook.data?.url;
  return (
    <Card title="Deploy webhook">
      <p class="text-dim">CI can deploy this service with a POST or GET to its webhook URL. Anyone who has the URL can deploy, so keep it secret.</p>
      {url && (
        <div class="mt-3 space-y-2">
          <CodeBlock label="Webhook URL" text={url} />
          <CodeBlock label="From CI" text={`curl -fsS -X POST "${url}"`} />
        </div>
      )}
      {hook.error &&
        (hook.error.status === 404 ? (
          <Note class="mt-3">{errText(hook.error)}</Note>
        ) : (
          <div class="mt-3">
            <ErrorBox error={hook.error} />
          </div>
        ))}
      {canOperate && !canRead && !url && <p class="mt-3 text-xs text-muted">Seeing the current URL needs the secrets:read grant. Rotating it shows you a new URL once.</p>}
      {canOperate && (
        <div class="mt-3">
          <Action
            confirm="Rotate the webhook URL? The current URL stops working right away."
            run={async () => hook.set(await post<{ url: string }>(`/services/${c.d.service.id}/hook`))}
            done="The webhook has a new URL."
          >
            Rotate URL
          </Action>
        </div>
      )}
    </Card>
  );
}

function Deploys({ c }: { c: Ctx }) {
  const id = c.d.service.id;
  const r = useApi<{ deploys: Deploy[]; live: string | null }>(`/services/${id}/deploys`, (e) => e.service_id === id && e.kind === "deploy");
  const canDeploy = atLeast(c.d.role, "deployer");
  return (
    <Card
      title="Deploys"
      actions={
        canDeploy && (
          <Action confirm="Roll back to the previous image?" run={() => rollback(c)}>
            Roll back
          </Action>
        )
      }
      pad={false}
    >
      <Loaded r={r} skeleton={<RowsSkeleton />}>
        {(data) => (
          <Rows items={data.deploys} empty="This service has not been deployed yet.">
            {(d) => (
              <li key={d.id} class="px-4 py-3 md:px-5">
                <DeployRow d={d} live={data.live} c={c} actions />
              </li>
            )}
          </Rows>
        )}
      </Loaded>
    </Card>
  );
}

/** A deploy's log, streamed live until the deploy finishes. */
export function DeployLog(props: { id: string | null; onClose: () => void }) {
  const [lines, setLines] = useState<{ ts: number; line: string }[]>([]);
  const [status, setStatus] = useState("");
  const [info, setInfo] = useState<Deploy | null>(null);
  const box = useStickToBottom<HTMLPreElement>(lines);
  const id = props.id;

  useEffect(() => {
    if (!id) return;
    setLines([]);
    setStatus("");
    setInfo(null);
    box.stick.current = true;
    const refresh = () =>
      get<{ deploy: Deploy }>(`/deploys/${id}`)
        .then((r) => {
          setInfo(r.deploy);
          setStatus((s) => s || r.deploy.status);
        })
        .catch(() => {});
    refresh();
    const es = new EventSource(`/api/deploys/${id}/log`);
    const tail = tailLines<{ ts: number; line: string }>(es, (chunk) => setLines((l) => l.concat(chunk)));
    // A reconnect replays the whole log, so start over when the stream (re)opens.
    es.onopen = () => {
      tail.clear();
      setLines([]);
    };
    es.addEventListener("status", (m) => {
      setStatus(m.data);
      // The server ends the stream after the final status; closing stops the browser from reconnecting.
      if (FINAL.has(m.data)) {
        es.close();
        refresh();
      }
    });
    return tail.stop;
  }, [id]);

  const done = FINAL.has(status);
  return (
    <Modal open={!!id} onClose={props.onClose} title="Deploy log" wide>
      <div class="mb-3 flex flex-wrap items-center gap-2">
        <Pill state={status || "queued"} />
        {info && <span class="text-dim">{started(info)}</span>}
        {id && !done && status && (
          <Action class="btn btn-danger ml-auto" run={() => post(`/deploys/${id}/cancel`)} done="Cancelling.">
            Cancel deploy
          </Action>
        )}
      </div>
      <pre
        ref={box.ref}
        tabIndex={0}
        aria-label="Deploy log"
        onScroll={box.onScroll}
        class="h-[60vh] overflow-auto whitespace-pre-wrap break-words rounded-md border border-line bg-bg p-3 font-mono text-xs leading-relaxed"
      >
        {lines.length ? lines.map((l) => `${clock(l.ts)}  ${l.line}`).join("\n") : done ? "This deploy wrote no log lines." : "Waiting for the first lines…"}
      </pre>
      {info?.error && done && (
        <div class="mt-3">
          <ErrorBox error={info.error} />
        </div>
      )}
    </Modal>
  );
}

function Env({ c }: { c: Ctx }) {
  return (
    <Card title="Environment">
      <EnvEditor
        path={`/services/${c.d.service.id}/env`}
        field="env"
        canEdit={atLeast(c.d.role, "operator")}
        savedNote={<RedeployNote c={c}>Saved. The new values reach the containers with the next deploy.</RedeployNote>}
        footer={(data) =>
          data.project_vars?.length > 0 && (
            <p class="border-t border-line pt-4 text-xs text-muted">
              The service also receives the project variables {data.project_vars.join(", ")}. A variable set here with the same name takes precedence.
            </p>
          )
        }
      />
      <p class="mt-4 text-xs text-muted">
        Every value is stored encrypted and reaches only this service's containers through memory-only storage, so it never shows up in docker inspect or image layers.
      </p>
    </Card>
  );
}

function Files({ c }: { c: Ctx }) {
  const id = c.d.service.id;
  const canEdit = atLeast(c.d.role, "operator");
  const r = useApi<{ files: { target: string; content?: string }[]; values_visible: boolean }>(`/services/${id}/files`);
  const [edit, setEdit] = useState<{ target: string; content: string; known: boolean; isNew: boolean } | null>(null);
  const [saved, setSaved] = useState(false);
  const s = useSubmit(async () => {
    if (!edit) return;
    await put(`/services/${id}/files`, { target: edit.target.trim(), content: edit.content });
    setEdit(null);
    setSaved(true);
    await r.reload();
  });
  return (
    <Card
      title="Files"
      actions={
        canEdit && (
          <button class="btn" onClick={() => setEdit({ target: "", content: "", known: true, isNew: true })}>
            New file
          </button>
        )
      }
    >
      <p class="mb-4 text-dim">Files are mounted into the containers at their path. Their contents are stored encrypted, like environment values.</p>
      {saved && (
        <Note tone="ok" class="mb-4">
          <RedeployNote c={c}>Saved. The containers get the new file with the next deploy.</RedeployNote>
        </Note>
      )}
      <Loaded r={r}>
        {(data) =>
          data.files.length === 0 ? (
            <Empty>There are no files yet.</Empty>
          ) : (
            <ul class="inset-list">
              {data.files.map((f) => (
                <li key={f.target} class="flex flex-wrap items-center gap-2 px-3 py-2">
                  <code class="min-w-0 flex-1 truncate">{f.target}</code>
                  {canEdit && (
                    <>
                      <IconButton icon="edit" label={`Edit ${f.target}`} onClick={() => setEdit({ target: f.target, content: f.content ?? "", known: data.values_visible, isNew: false })} />
                      <Action
                        class="btn btn-ghost btn-sm px-2 text-bad"
                        label={`Remove ${f.target}`}
                        confirm={`Remove ${f.target}?`}
                        run={async () => {
                          await put(`/services/${id}/files`, { target: f.target, content: null });
                          setSaved(true);
                          await r.reload();
                        }}
                      >
                        <Icon name="trash" />
                      </Action>
                    </>
                  )}
                </li>
              ))}
            </ul>
          )
        }
      </Loaded>
      <Modal open={!!edit} onClose={() => setEdit(null)} title={edit?.isNew ? "New file" : edit?.target} wide>
        {edit && (
          <form class="space-y-4" onSubmit={s.submit}>
            {edit.isNew && (
              <Input
                label="Path in the container"
                class="font-mono"
                required
                pattern="/.*"
                placeholder="/etc/app/config.yml"
                value={edit.target}
                onInput={(e) => ((target) => setEdit((x) => x && { ...x, target }))(e.currentTarget.value)}
              />
            )}
            {!edit.known && <Note>The current content is hidden because reading it needs the secrets:read grant. Saving replaces it.</Note>}
            <TextArea label="Content" class="h-72" value={edit.content} onInput={(e) => ((content) => setEdit((x) => x && { ...x, content }))(e.currentTarget.value)} />
            <FormButtons s={s} submit="Save file" onCancel={() => setEdit(null)} />
          </form>
        )}
      </Modal>
    </Card>
  );
}

function Danger({ c }: { c: Ctx }) {
  const me = useMe();
  const s = c.d.service;
  const [name, setName] = useState("");
  const [volumes, setVolumes] = useState(false);
  // A primary database with read replicas can go only after they are promoted or deleted.
  const replicas = useApi<{ replicas: { name: string }[] }>(s.kind === "database" && !s.spec.database?.primary ? `/services/${s.id}/replicas` : null);
  const followers = replicas.data?.replicas.map((x) => x.name) ?? [];
  const canDestroy = atLeast(c.d.role, "admin") && hasGrant(me, "destroy");
  const allowed = canDestroy && followers.length === 0;
  const sub = useSubmit(async () => {
    await del(`/services/${s.id}${volumes ? "?volumes=true" : ""}`);
    toast(`Deleted ${s.name}.`);
    route(`/projects/${s.project}`);
  });
  return (
    <Card title="Delete service" class="border-bad/40">
      <form class="max-w-xl space-y-4" onSubmit={sub.submit}>
        <p class="text-dim">Deleting stops and removes every container of {s.name}, together with its domains, environment, files and deploy history. This cannot be undone.</p>
        {followers.length > 0 && (
          <Note tone="warn">
            {followers.join(", ")} {followers.length === 1 ? "follows" : "follow"} {s.name}, so {s.name} can be deleted only after each read replica is promoted or deleted.
          </Note>
        )}
        {!canDestroy && <Note tone="warn">Deleting needs the admin role on this project and the destroy grant.</Note>}
        <Check
          checked={volumes}
          onChange={setVolumes}
          disabled={!allowed}
          label="Also delete its volumes"
          hint="Volumes hold the service's data, such as database files. Leave this off to keep them on the server."
        />
        {volumes && <Note tone="bad">The data in this service's volumes will be gone for good, so take a backup first if you might need it.</Note>}
        <ConfirmName name={s.name} value={name} onChange={setName} disabled={!allowed} />
        <ErrorBox error={sub.error} />
        <button class="btn btn-danger" disabled={!allowed || name !== s.name || sub.busy}>
          Delete {s.name}
          {volumes ? " and its volumes" : ""}
        </button>
      </form>
    </Card>
  );
}
