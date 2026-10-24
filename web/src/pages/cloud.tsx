// Cloud accounts, the machines Tugport creates in them, and server pools. Owner only: these spend money.
import { useRef, useState } from "preact/hooks";
import { del, patch, post, useApi, useMe, type Event } from "../api";
import { Icon } from "../icons";
import {
  Action,
  Card,
  ErrorBox,
  Field,
  FormButtons,
  IconButton,
  Input,
  Loaded,
  Loading,
  Modal,
  NameInput,
  Note,
  Page,
  Pill,
  Rows,
  Select,
  Tabs,
  TextArea,
  toast,
  useForm,
  useSubmit,
} from "../ui";
import { ago, labelsText, requireLabels, sentence, sizeLabel, sizesIn, type MachineSize } from "../util";

export interface Account {
  id: string;
  kind: string;
  name: string;
  created_at: number;
}
export interface Machine {
  id: string;
  account_id: string;
  pool_id: string | null;
  name: string;
  region: string;
  size: string;
  node_id: string | null;
  status: "creating" | "joined" | "failed";
  error: string | null;
  created_at: number;
}
export interface Pool {
  id: string;
  name: string;
  account_id: string;
  region: string;
  size: string;
  labels: Record<string, string>;
  min_servers: number;
  max_servers: number;
  scale_up_cpu: number;
  scale_down_cpu: number;
}
type Options = { regions: { id: string; name: string }[]; sizes: MachineSize[] };

export const PROVIDERS: Record<string, string> = { hetzner: "Hetzner", aws: "AWS" };
const providerName = (a: Account) => PROVIDERS[a.kind] ?? a.kind;
/** The provider of an account, by its id. */
export const providerOf = (accounts: Account[] | undefined, id: string | undefined) => {
  const a = accounts?.find((x) => x.id === id);
  return a ? providerName(a) : "the provider";
};
const accountOptions = (accounts: Account[]) => accounts.map((a): [string, string] => [a.id, `${a.name} (${providerName(a)})`]);
/** "at Hetzner in the account main" */
const accountText = (accounts: Account[] | undefined, id: string) => {
  const a = accounts?.find((x) => x.id === id);
  return a ? `at ${providerName(a)} in the account ${a.name}` : "in an account that was removed";
};

/** The tabs over the servers pages; the Cloud tab is the owner's alone. */
export function ServerTabs(props: { current: "list" | "cloud" }) {
  if (!useMe().principal.is_owner) return null;
  return (
    <Tabs
      label="Server sections"
      current={props.current}
      items={[
        ["list", "Servers", "/servers"],
        ["cloud", "Cloud", "/servers/cloud"],
      ]}
    />
  );
}

/**
 * The owner's cloud machines and accounts (null for everyone else). Creation and joins arrive as
 * "nodes" events, but a machine that fails or is deleted for never joining does not, so a list
 * with machines still setting up also polls.
 */
export function useCloud() {
  const owner = useMe().principal.is_owner;
  const settingUp = useRef(false);
  const machines = useApi<{ servers: Machine[] }>(owner ? "/cloud/servers" : null, (e: Event) => e.kind === "nodes", settingUp.current ? 15000 : undefined);
  settingUp.current = !!machines.data?.servers.some((m) => m.status === "creating");
  const accounts = useApi<{ accounts: Account[] }>(owner ? "/cloud/accounts" : null);
  return { machines, accounts };
}

/** "a cx22 in fsn1, at Hetzner in the account main" */
export const machineText = (m: Machine, accounts: Account[] | undefined) => `a ${m.size} in ${m.region}, ${accountText(accounts, m.account_id)}`;

export function CloudPage() {
  const me = useMe();
  const { machines, accounts } = useCloud();
  const pools = useApi<{ pools: Pool[] }>(me.principal.is_owner ? "/cloud/pools" : null);
  const [dialog, setDialog] = useState<"account" | "server" | { pool?: Pool } | null>(null);
  if (!me.principal.is_owner) {
    return (
      <Page title="Servers">
        <Note>Only the owner can manage cloud accounts and the servers Tugport creates in them.</Note>
      </Page>
    );
  }
  const accs = accounts.data?.accounts ?? [];
  const close = () => setDialog(null);
  return (
    <Page
      title="Servers"
      sub="Tugport can create servers at Hetzner or AWS. They install Tugport and join by themselves, and they cost money at the provider until you remove them."
    >
      <ServerTabs current="cloud" />
      <div class="space-y-6">
        <Card
          title="Cloud accounts"
          pad={false}
          actions={
            <button class="btn" onClick={() => setDialog("account")}>
              Add an account
            </button>
          }
        >
          <Loaded r={accounts}>
            {(d) => (
              <Rows items={d.accounts} empty="Add a Hetzner or AWS account, and Tugport can create servers in it that join this panel by themselves.">
                {(a) => (
                  <li key={a.id} class="row">
                    <span class="min-w-0 flex-1">
                      <span class="font-medium">{a.name}</span> <span class="chip">{providerName(a)}</span>
                      <span class="block text-xs text-muted">Added {ago(a.created_at)}</span>
                    </span>
                    <Action
                      class="btn btn-ghost btn-sm px-2 text-bad"
                      label={`Remove the account ${a.name}`}
                      confirm={`Remove the account ${a.name}? Tugport forgets its credentials. An account that still has machines or pools cannot be removed.`}
                      run={() => del(`/cloud/accounts/${a.id}`).then(accounts.reload)}
                      done="Removed the account."
                    >
                      <Icon name="trash" />
                    </Action>
                  </li>
                )}
              </Rows>
            )}
          </Loaded>
        </Card>

        <Card
          title="Machines"
          pad={false}
          actions={
            <button class="btn btn-primary" disabled={!accs.length} onClick={() => setDialog("server")}>
              Create a server
            </button>
          }
        >
          <p class="px-4 pt-4 text-dim">
            These are the machines Tugport created. Once one joins, it is a server like any other, and removing it from its server page deletes the machine as well.
          </p>
          <Loaded r={machines}>
            {(d) => (
              <Rows
                items={d.servers}
                class="mt-3 border-t border-line"
                empty={accs.length ? "Tugport has not created any machines yet." : "Add a cloud account first, and then Tugport can create servers in it."}
              >
                {(m) => <MachineRow key={m.id} m={m} accounts={accs} pools={pools.data?.pools} onChanged={machines.reload} />}
              </Rows>
            )}
          </Loaded>
        </Card>

        <Card
          title="Server pools"
          pad={false}
          actions={
            <button class="btn" disabled={!accs.length} onClick={() => setDialog({})}>
              New pool
            </button>
          }
        >
          <p class="px-4 pt-4 text-dim">
            A pool adds servers when the load is high and removes them when it is low, within limits you set. Every server it adds costs money at the provider.
          </p>
          <Loaded r={pools}>
            {(d) => (
              <Rows items={d.pools} class="mt-3 border-t border-line" empty="There are no pools yet.">
                {(p) => {
                  const members = (machines.data?.servers ?? []).filter((m) => m.pool_id === p.id && m.status !== "failed").length;
                  return (
                    <li key={p.id} class="row">
                      <div class="min-w-0 flex-1 space-y-1">
                        <div class="flex flex-wrap items-center gap-2">
                          <span class="font-medium">{p.name}</span>
                          <span class="chip">
                            {members} {members === 1 ? "server" : "servers"} now
                          </span>
                          {Object.entries(p.labels ?? {}).map(([k, v]) => (
                            <span key={k} class="chip font-mono">{`${k}=${v}`}</span>
                          ))}
                        </div>
                        <p class="text-xs text-muted">
                          It keeps {p.min_servers} to {p.max_servers} servers of size {p.size} in {p.region}, {accountText(accs, p.account_id)}, and it grows above {p.scale_up_cpu}
                          % CPU and shrinks below {p.scale_down_cpu}%.
                        </p>
                      </div>
                      <span class="flex gap-2">
                        <IconButton icon="edit" label={`Edit the pool ${p.name}`} onClick={() => setDialog({ pool: p })} />
                        <Action
                          class="btn btn-ghost btn-sm px-2 text-bad"
                          label={`Delete the pool ${p.name}`}
                          confirm={`Delete the pool ${p.name}? Its servers stay and keep costing money, and Tugport stops adding or removing servers for it. To remove the servers as well, set the fewest and most servers to 0 first and wait until the pool has shrunk.`}
                          run={() => del(`/cloud/pools/${p.id}`).then(pools.reload)}
                          done="Deleted the pool. Its servers stay."
                        >
                          <Icon name="trash" />
                        </Action>
                      </span>
                    </li>
                  );
                }}
              </Rows>
            )}
          </Loaded>
        </Card>
      </div>
      {dialog === "account" && <AddAccount onClose={close} onSaved={accounts.reload} />}
      {dialog === "server" && <CreateServer accounts={accs} onClose={close} onCreated={machines.reload} />}
      {dialog && typeof dialog === "object" && <PoolForm pool={dialog.pool} accounts={accs} onClose={close} onSaved={pools.reload} />}
    </Page>
  );
}

const STATUS: Record<Machine["status"], string> = { creating: "setting up", joined: "joined", failed: "failed" };

function MachineRow(props: { m: Machine; accounts: Account[]; pools: Pool[] | undefined; onChanged: () => void }) {
  const { m } = props;
  const pool = props.pools?.find((p) => p.id === m.pool_id);
  const provider = providerOf(props.accounts, m.account_id);
  return (
    <li class="row">
      <div class="min-w-0 flex-1 space-y-1">
        <div class="flex flex-wrap items-center gap-2">
          {m.node_id ? (
            <a class="link font-medium" href={`/servers/${m.node_id}`}>
              {m.name}
            </a>
          ) : (
            <span class="font-medium">{m.name}</span>
          )}
          <Pill state={m.status} label={STATUS[m.status] ?? m.status} />
          {pool && <span class="chip">pool {pool.name}</span>}
        </div>
        <p class="text-xs text-muted">{sentence(`${machineText(m, props.accounts)}, created ${ago(m.created_at)}`)}</p>
        {m.status === "creating" && (
          <p class="text-xs text-dim">It installs Tugport and joins within a few minutes. If it has not joined 20 minutes after it was created, Tugport deletes it.</p>
        )}
        {m.error && <p class="text-xs text-bad">{m.error}</p>}
      </div>
      {!m.node_id && (
        <Action
          class="btn btn-ghost btn-sm px-2 text-bad"
          label={`Delete ${m.name}`}
          confirm={`Delete ${m.name} at ${provider}? It has not joined, so nothing runs on it.`}
          run={() => del(`/cloud/servers/${m.id}`).then(props.onChanged)}
          done="Deleted the machine."
        >
          <Icon name="trash" />
        </Action>
      )}
    </li>
  );
}

function AddAccount(props: { onClose: () => void; onSaved: () => void }) {
  const { f, set, bind } = useForm({ kind: "hetzner", name: "", token: "", access: "", secret: "" });
  const sub = useSubmit(async () => {
    const creds = f.kind === "hetzner" ? { token: f.token.trim() } : { access_key: f.access.trim(), secret_key: f.secret.trim() };
    await post("/cloud/accounts", { kind: f.kind, name: f.name.trim(), ...creds });
    toast(`Added the account ${f.name.trim()}.`);
    props.onSaved();
    props.onClose();
  });
  const secret = (label: string, k: "token" | "access" | "secret", hint?: string) => (
    <Input label={label} hint={hint} mono type="password" required autoComplete="off" {...bind(k)} />
  );
  return (
    <Modal open onClose={props.onClose} title="Add a cloud account">
      <form class="space-y-4" onSubmit={sub.submit}>
        <div class="grid gap-4 sm:grid-cols-2">
          <Field label="Provider">
            <Select
              value={f.kind}
              onChange={(kind) => set({ kind })}
              options={[
                ["hetzner", "Hetzner Cloud"],
                ["aws", "Amazon Web Services"],
              ]}
            />
          </Field>
          <Input label="Name" hint="A name helps you tell accounts apart." required placeholder={f.kind === "aws" ? "aws-main" : "hetzner-main"} {...bind("name")} />
        </div>
        {f.kind === "hetzner" ? (
          secret("API token", "token", "Create a token with read and write access in the Hetzner Cloud console, under Security.")
        ) : (
          <>
            {secret("Access key ID", "access")}
            {secret("Secret access key", "secret", "Use the keys of an IAM user that may describe regions and run, describe and terminate EC2 instances.")}
          </>
        )}
        <p class="text-xs text-muted">Tugport checks the credentials with the provider before it saves them, and it stores them encrypted.</p>
        <FormButtons s={sub} submit={sub.busy ? "Checking the credentials…" : "Add the account"} onCancel={props.onClose} />
      </form>
    </Modal>
  );
}

/** A region and size from what the account offers: the first ones on offer until someone chooses. */
function useMachineChoice(account: string, init?: { region: string; size: string }) {
  const [pick, setPick] = useState(init ?? { region: "", size: "" });
  const opts = useApi<Options>(account ? `/cloud/accounts/${account}/options` : null);
  const region = pick.region || opts.data?.regions[0]?.id || "";
  const sizes = sizesIn(opts.data?.sizes ?? [], region);
  const size = sizes.some((z) => z.id === pick.size) ? pick.size : (sizes[0]?.id ?? "");
  return { opts, sizes, region, size, set: (p: { region?: string; size?: string }) => setPick((x) => ({ ...x, ...p })), reset: () => setPick({ region: "", size: "" }) };
}

function MachineFields({ choice }: { choice: ReturnType<typeof useMachineChoice> }) {
  const { opts, sizes } = choice;
  if (opts.error) return <ErrorBox error={opts.error} />;
  if (!opts.data) return <Loading />;
  const hourly = Number(sizes.find((z) => z.id === choice.size)?.prices?.find((p) => p.region === choice.region)?.hourly ?? NaN);
  return (
    <div class="grid gap-4 sm:grid-cols-[1fr_2fr]">
      <Field label="Region">
        <Select value={choice.region} onChange={(region) => choice.set({ region })} options={opts.data.regions.map((r): [string, string] => [r.id, `${r.name} · ${r.id}`])} />
      </Field>
      <Field
        label="Size"
        hint={isNaN(hourly) ? undefined : `It costs €${hourly.toFixed(4)} an hour at Hetzner with VAT, which is about €${(hourly * 730).toFixed(2)} for a full month.`}
      >
        <Select value={choice.size} onChange={(size) => choice.set({ size })} options={sizes.map((z): [string, string] => [z.id, sizeLabel(z, choice.region)])} />
      </Field>
    </div>
  );
}

function CreateServer(props: { accounts: Account[]; onClose: () => void; onCreated: () => void }) {
  const [account, setAccount] = useState(props.accounts[0]?.id ?? "");
  const [url, setUrl] = useState("");
  const choice = useMachineChoice(account);
  const provider = providerOf(props.accounts, account);
  const sub = useSubmit(async () => {
    if (!choice.region || !choice.size) throw new Error("Pick a region and a size first.");
    await post("/cloud/servers", { account_id: account, region: choice.region, size: choice.size, panel_url: url.trim().replace(/\/+$/, "") || undefined });
    toast(`${provider} is starting the machine. It joins within a few minutes.`);
    props.onCreated();
    props.onClose();
  });
  return (
    <Modal open onClose={props.onClose} title="Create a server" wide>
      <form class="space-y-4" onSubmit={sub.submit}>
        <p class="text-dim">Tugport creates a machine with Ubuntu, which installs Tugport and joins this panel by itself within a few minutes.</p>
        <Field label="Account">
          <Select
            value={account}
            onChange={(a) => {
              setAccount(a);
              choice.reset();
            }}
            options={accountOptions(props.accounts)}
          />
        </Field>
        <MachineFields choice={choice} />
        <Input
          label="Panel address (optional)"
          hint="The machine calls the panel at this address over the internet to join. Leave this empty to use the panel domain, or this server's public address when there is no domain."
          mono
          type="url"
          placeholder="https://deploy.example.com"
          value={url}
          onInput={(e) => setUrl(e.currentTarget.value)}
        />
        <Note tone="warn">
          This creates a machine at {provider} that costs money there, by the hour, until you remove it. If it has not joined within 20 minutes, Tugport deletes it.
        </Note>
        <FormButtons s={sub} submit={sub.busy ? "Creating the machine…" : "Create the server"} onCancel={props.onClose} />
      </form>
    </Modal>
  );
}

function PoolForm(props: { pool?: Pool; accounts: Account[]; onClose: () => void; onSaved: () => void }) {
  const p = props.pool;
  const { f, set, bind } = useForm({
    name: p?.name ?? "",
    account: p?.account_id ?? props.accounts[0]?.id ?? "",
    labels: labelsText(p?.labels),
    min: String(p?.min_servers ?? 0),
    max: String(p?.max_servers ?? 3),
    up: String(p?.scale_up_cpu ?? 75),
    down: String(p?.scale_down_cpu ?? 25),
  });
  const choice = useMachineChoice(f.account, p && { region: p.region, size: p.size });
  const provider = providerOf(props.accounts, f.account);
  const sub = useSubmit(async () => {
    const labels = requireLabels(f.labels);
    const body = {
      region: choice.region,
      size: choice.size,
      labels,
      min_servers: Number(f.min),
      max_servers: Number(f.max),
      scale_up_cpu: Number(f.up),
      scale_down_cpu: Number(f.down),
    };
    if (p) await patch(`/cloud/pools/${p.id}`, body);
    else await post("/cloud/pools", { ...body, name: f.name, account_id: f.account });
    toast(p ? `Saved the pool ${p.name}.` : `Created the pool ${f.name}.`);
    props.onSaved();
    props.onClose();
  });
  const num = (label: string, k: "min" | "max" | "up" | "down") => <Input label={label} type="number" required min={0} max={100} step="any" {...bind(k)} />;
  return (
    <Modal open onClose={props.onClose} title={p ? `Edit the pool ${p.name}` : "New server pool"} wide>
      <form class="space-y-4" onSubmit={sub.submit}>
        {!p && (
          <div class="grid gap-4 sm:grid-cols-2">
            <Field label="Name" hint="Use lowercase letters, digits and dashes.">
              <NameInput value={f.name} onChange={(name) => set({ name })} />
            </Field>
            <Field label="Account">
              <Select
                value={f.account}
                onChange={(account) => {
                  set({ account });
                  choice.reset();
                }}
                options={accountOptions(props.accounts)}
              />
            </Field>
          </div>
        )}
        <MachineFields choice={choice} />
        <TextArea
          label="Labels (optional)"
          hint="Every server in the pool gets these labels when it joins, so services can ask for pool servers in their placement."
          class="h-16"
          placeholder="pool=web"
          {...bind("labels")}
        />
        <div class="grid grid-cols-2 gap-4 sm:grid-cols-4">
          {num("Fewest servers", "min")}
          {num("Most servers", "max")}
          {num("Grow above CPU (%)", "up")}
          {num("Shrink below CPU (%)", "down")}
        </div>
        <p class="text-dim">
          {Number(f.min) > 0 ? `The pool keeps at least ${f.min} ${Number(f.min) === 1 ? "server" : "servers"}.` : "The pool can shrink to no servers at all."} While the average
          CPU of all servers stays above {f.up || 0}%, it adds one server at a time, up to {f.max || 0}, and waits 10 minutes between additions. After the CPU stays below{" "}
          {f.down || 0}% for 30 minutes, it removes one, newest first. It never removes a server that keeps data, such as one running a database.
        </p>
        <Note tone="warn">Every server the pool adds costs money at {provider} until the pool or you remove it.</Note>
        <FormButtons s={sub} submit={p ? "Save the pool" : "Create the pool"} onCancel={props.onClose} />
      </form>
    </Modal>
  );
}
