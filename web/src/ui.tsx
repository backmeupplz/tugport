// Small shared components. Native elements first: <dialog> for modals, confirm() for
// quick confirmations, <fieldset disabled> for read-only forms.
import type { ComponentChildren, JSX } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import type { ApiError } from "./api";
import { Icon, type IconName } from "./icons";
import { copyText, sentence } from "./util";

export const errText = (e: unknown) => (e instanceof Error ? sentence(e.message) : "Something went wrong.");

/**
 * A page: breadcrumbs (label and link pairs leading here), the title with `meta` (pills, chips)
 * beside it, a line of `sub` text and the page's main actions.
 */
export function Page(props: {
  title: ComponentChildren;
  crumbs?: [string, string][];
  meta?: ComponentChildren;
  sub?: ComponentChildren;
  actions?: ComponentChildren;
  children?: ComponentChildren;
}) {
  return (
    <div class="mx-auto w-full max-w-6xl px-4 py-6 md:px-10 md:py-9">
      {props.crumbs && (
        <nav aria-label="Breadcrumb" class="mb-2 flex flex-wrap items-center gap-1.5 text-[13px] text-muted">
          {props.crumbs.map(([label, href], i) => (
            <span key={href} class="flex items-center gap-1.5">
              {i > 0 && <span aria-hidden="true">/</span>}
              <a class="hover:text-text" href={href}>
                {label}
              </a>
            </span>
          ))}
        </nav>
      )}
      <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
        <div class="min-w-0">
          <div class="flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <h1 class="break-words text-2xl font-semibold tracking-tight">{props.title}</h1>
            {props.meta}
          </div>
          {props.sub && <div class="mt-1.5 max-w-3xl text-dim">{props.sub}</div>}
        </div>
        {props.actions && <div class="flex flex-wrap gap-2">{props.actions}</div>}
      </header>
      {props.children}
    </div>
  );
}

export function Card(props: { title?: ComponentChildren; actions?: ComponentChildren; children?: ComponentChildren; class?: string; pad?: boolean }) {
  return (
    <section class={`card min-w-0 ${props.class ?? ""}`}>
      {(props.title || props.actions) && (
        <header class="flex min-h-13 flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-2.5 md:px-5">
          <h2 class="text-sm font-semibold">{props.title}</h2>
          {props.actions && <div class="flex flex-wrap gap-2">{props.actions}</div>}
        </header>
      )}
      <div class={props.pad === false ? "" : "p-4 md:p-5"}>{props.children}</div>
    </section>
  );
}

/**
 * The "i" beside a label that shows what a setting does. It sits inside the <label>, so it is a
 * span (not a labelable button, which would take the label's clicks from the control).
 */
function InfoToggle(props: { open: boolean; onToggle: () => void }) {
  const toggle = (e: Event) => {
    e.preventDefault();
    e.stopPropagation();
    props.onToggle();
  };
  return (
    <span
      role="button"
      tabIndex={0}
      aria-expanded={props.open}
      aria-label="What this does"
      title="What this does"
      class={`inline-grid size-5 cursor-pointer place-items-center rounded-full align-middle hover:text-text ${props.open ? "text-accent" : "text-muted"}`}
      onClick={toggle}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && toggle(e)}
    >
      <Icon name="info" class="size-3.5" />
    </span>
  );
}

const InfoText = (props: { children: ComponentChildren }) => <span class="mb-2 block rounded-lg bg-raised px-3 py-2 text-xs leading-relaxed text-dim">{props.children}</span>;

/**
 * A labelled control. The label wraps the control, which associates them without ids. `info`
 * explains the setting behind an "i"; `hint` stays visible under the control.
 */
export function Field(props: { label: ComponentChildren; hint?: ComponentChildren; info?: ComponentChildren; children: ComponentChildren; class?: string }) {
  const [open, setOpen] = useState(false);
  return (
    <label class={`block min-w-0 ${props.class ?? ""}`}>
      <span class="label">
        {props.label}
        {props.info && (
          <>
            {" "}
            <InfoToggle open={open} onToggle={() => setOpen(!open)} />
          </>
        )}
      </span>
      {open && <InfoText>{props.info}</InfoText>}
      {props.children}
      {props.hint && <span class="hint">{props.hint}</span>}
    </label>
  );
}

type Labelled = { label: ComponentChildren; hint?: ComponentChildren; info?: ComponentChildren; fieldClass?: string };

/** A labelled text input. `mono` suits names, paths and commands: monospace and no spellcheck. */
export function Input({ label, hint, info, fieldClass, mono, ...rest }: Labelled & { mono?: boolean } & JSX.IntrinsicElements["input"]) {
  return (
    <Field label={label} hint={hint} info={info} class={fieldClass}>
      <input spellcheck={mono ? false : undefined} {...rest} class={`input${mono ? " font-mono" : ""}${rest.class ? ` ${rest.class}` : ""}`} />
    </Field>
  );
}

/** A labelled textarea for lists, files and code, so without spellcheck. `class` sets its height. */
export function TextArea({ label, hint, info, fieldClass, ...rest }: Labelled & JSX.IntrinsicElements["textarea"]) {
  return (
    <Field label={label} hint={hint} info={info} class={fieldClass}>
      <textarea spellcheck={false} {...rest} class={`input ${rest.class ?? ""}`} />
    </Field>
  );
}

/** The 6-digit code from an authenticator app. */
export const CodeInput = (props: JSX.IntrinsicElements["input"]) => (
  <input class="input h-11 text-center font-mono text-lg tracking-[0.4em]" required inputMode="numeric" autoComplete="one-time-code" pattern="[0-9]{6}" maxLength={6} {...props} />
);

/** "Type the name to confirm" in front of something that cannot be undone. */
export function ConfirmName(props: { name: string; value: string; onChange: (v: string) => void; disabled?: boolean }) {
  return (
    <Input label={`Type ${props.name} to confirm`} mono autoComplete="off" disabled={props.disabled} value={props.value} onInput={(e) => props.onChange(e.currentTarget.value)} />
  );
}

export function Check(props: { checked: boolean; onChange: (v: boolean) => void; label: ComponentChildren; hint?: ComponentChildren; info?: ComponentChildren; disabled?: boolean }) {
  const [open, setOpen] = useState(false);
  return (
    <label class="flex items-start gap-2.5">
      <input type="checkbox" class="mt-0.5 size-4 shrink-0" checked={props.checked} disabled={props.disabled} onChange={(e) => props.onChange(e.currentTarget.checked)} />
      <span class="min-w-0">
        <span class="font-medium">{props.label}</span>
        {props.info && (
          <>
            {" "}
            <InfoToggle open={open} onToggle={() => setOpen(!open)} />
          </>
        )}
        {open && <span class="mt-1.5 block">{<InfoText>{props.info}</InfoText>}</span>}
        {props.hint && <span class="hint">{props.hint}</span>}
      </span>
    </label>
  );
}

/** A square button with just an icon; `label` is its accessible name and tooltip. */
export function IconButton(props: { icon: IconName; label: string; onClick?: () => void; class?: string; disabled?: boolean; href?: string }) {
  const cls = `btn btn-ghost btn-sm px-2 ${props.class ?? ""}`;
  return props.href ? (
    <a class={cls} href={props.href} aria-label={props.label} title={props.label}>
      <Icon name={props.icon} />
    </a>
  ) : (
    <button type="button" class={cls} aria-label={props.label} title={props.label} disabled={props.disabled} onClick={props.onClick}>
      <Icon name={props.icon} />
    </button>
  );
}

export function Select(props: { value: string; onChange: (v: string) => void; options: (string | [string, string])[]; class?: string; disabled?: boolean; "aria-label"?: string }) {
  return (
    <select
      class={`input ${props.class ?? ""}`}
      value={props.value}
      disabled={props.disabled}
      aria-label={props["aria-label"]}
      onChange={(e) => props.onChange(e.currentTarget.value)}
    >
      {props.options.map((o) => {
        const [v, l] = Array.isArray(o) ? o : [o, o];
        return (
          <option key={v} value={v}>
            {l}
          </option>
        );
      })}
    </select>
  );
}

/** A DNS-safe name (projects, services, servers): lowercase letters, digits and dashes. An unchanged `initial` name passes as it is. */
export function NameInput(props: { value: string; onChange: (v: string) => void; autoFocus?: boolean; initial?: string }) {
  return (
    <input
      class="input font-mono"
      required
      autoFocus={props.autoFocus ?? true}
      autoComplete="off"
      spellcheck={false}
      maxLength={40}
      pattern={props.value === props.initial ? undefined : "[a-z]([a-z0-9\\-]*[a-z0-9])?"}
      title="Lowercase letters, digits and dashes, starting with a letter"
      value={props.value}
      onInput={(e) => props.onChange(e.currentTarget.value.toLowerCase().replace(/[^a-z0-9-]/g, "-"))}
    />
  );
}

/**
 * Form state with `bind("field")` for text inputs, selects and textareas. Updates merge into the
 * latest state, because autofill fires several input events before the next render.
 */
export function useForm<T extends Record<string, any>>(init: T) {
  const [f, setF] = useState(init);
  const set = (p: Partial<T> | ((s: T) => Partial<T>)) => setF((s) => ({ ...s, ...(typeof p === "function" ? p(s) : p) }));
  const bind = (k: keyof T) => ({
    value: f[k] as string,
    onInput: (e: JSX.TargetedEvent<HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement>) => set({ [k]: e.currentTarget.value } as Partial<T>),
  });
  return { f, set, bind, reset: () => setF(init) };
}

/** Busy state and an inline error for a form's submit handler. */
export function useSubmit(fn: () => Promise<unknown>) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const submit = async (e?: Event) => {
    e?.preventDefault();
    setBusy(true);
    setError("");
    try {
      await fn();
    } catch (err) {
      setError(errText(err));
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, submit, setError };
}

/** A form's error and its Cancel (a button or a link) and submit buttons. */
export function FormButtons(props: {
  s: { busy: boolean; error?: string };
  submit: ComponentChildren;
  onCancel?: () => void;
  cancelHref?: string;
  cancel?: ComponentChildren;
  danger?: boolean;
  disabled?: boolean;
}) {
  return (
    <>
      <ErrorBox error={props.s.error} />
      <div class="flex justify-end gap-2">
        {props.cancelHref ? (
          <a class="btn" href={props.cancelHref}>
            Cancel
          </a>
        ) : (
          <button type="button" class="btn" onClick={props.onCancel}>
            {props.cancel ?? "Cancel"}
          </button>
        )}
        <button class={`btn ${props.danger ? "btn-danger" : "btn-primary"}`} disabled={props.s.busy || props.disabled}>
          {props.submit}
        </button>
      </div>
    </>
  );
}

/** The bar that sticks under a long settings form while it has unsaved changes. */
export function UnsavedBar(props: { s: { busy: boolean; error: string }; onDiscard: () => void }) {
  return (
    <div class="sticky bottom-0 z-10 -mx-4 mt-6 flex flex-wrap items-center justify-between gap-2 border-t border-line bg-surface px-4 py-3 md:mx-0 md:rounded-lg md:border">
      <span class="text-dim">You have unsaved changes.</span>
      <span class="flex gap-2">
        <button type="button" class="btn" onClick={props.onDiscard}>
          Discard
        </button>
        <button class="btn btn-primary" disabled={props.s.busy}>
          Save settings
        </button>
      </span>
      {props.s.error && (
        <div class="w-full">
          <ErrorBox error={props.s.error} />
        </div>
      )}
    </div>
  );
}

/**
 * A button that runs an API call, disables itself meanwhile and reports the outcome as a toast.
 * `label` names an icon-only button.
 */
export function Action(props: { run: () => Promise<unknown>; children: ComponentChildren; class?: string; confirm?: string; done?: string; disabled?: boolean; title?: string; label?: string }) {
  const [busy, setBusy] = useState(false);
  return (
    <button
      type="button"
      class={props.class ?? "btn"}
      title={props.title ?? props.label}
      aria-label={props.label}
      disabled={busy || props.disabled}
      onClick={async () => {
        if (props.confirm && !window.confirm(props.confirm)) return;
        setBusy(true);
        try {
          await props.run();
          if (props.done) toast(props.done);
        } catch (e) {
          toast(errText(e), "bad");
        } finally {
          setBusy(false);
        }
      }}
    >
      {props.children}
    </button>
  );
}

/** A ref for a <dialog> that shows as a modal while `open`. */
export function useDialog(open: boolean) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (open && !d.open) d.showModal();
    if (!open && d.open) d.close();
  }, [open]);
  return ref;
}

export function Modal(props: { open: boolean; onClose: () => void; title: ComponentChildren; children?: ComponentChildren; wide?: boolean }) {
  const ref = useDialog(props.open);
  return (
    <dialog
      ref={ref}
      onClose={props.onClose}
      class={`m-auto max-h-[calc(100dvh-2rem)] w-[calc(100%-2rem)] overflow-y-auto rounded-2xl border border-line bg-surface p-0 text-text shadow-2xl ${props.wide ? "max-w-3xl" : "max-w-lg"}`}
    >
      {props.open && (
        <div>
          <header class="sticky top-0 z-10 flex items-center justify-between gap-3 border-b border-line bg-surface px-5 py-3 md:px-6 md:py-3.5">
            <h2 class="min-w-0 truncate text-base font-semibold">{props.title}</h2>
            <button type="button" class="btn btn-ghost -mr-2 px-2" aria-label="Close" onClick={props.onClose}>
              <Icon name="close" />
            </button>
          </header>
          <div class="p-5 md:p-6">{props.children}</div>
        </div>
      )}
    </dialog>
  );
}

export function CopyButton(props: { text: string; label?: string; class?: string }) {
  const [state, setState] = useState<"" | "ok" | "fail">("");
  return (
    <button
      type="button"
      class={props.class ?? "btn btn-sm"}
      onClick={async () => {
        setState((await copyText(props.text)) ? "ok" : "fail");
        setTimeout(() => setState(""), 1600);
      }}
    >
      {state === "ok" ? "Copied" : state === "fail" ? "Select and copy" : (props.label ?? "Copy")}
    </button>
  );
}

export function CodeBlock(props: { text: string; label?: ComponentChildren }) {
  return (
    <div class="min-w-0 rounded-lg border border-line bg-bg">
      <div class="flex items-center justify-between gap-2 border-b border-line py-1.5 pr-1.5 pl-3 text-xs text-dim">
        <span class="min-w-0 truncate">{props.label}</span>
        <CopyButton text={props.text} />
      </div>
      <pre class="max-h-72 overflow-auto whitespace-pre-wrap break-all p-3 font-mono text-[13px] leading-snug">{props.text}</pre>
    </div>
  );
}

export function Empty(props: { title?: ComponentChildren; children?: ComponentChildren; action?: ComponentChildren }) {
  return (
    <div class="rounded-xl border border-dashed border-line-strong px-6 py-12 text-center">
      {props.title && <p class="font-medium">{props.title}</p>}
      {props.children && <p class="mx-auto mt-1 max-w-md text-dim">{props.children}</p>}
      {props.action && <div class="mt-4">{props.action}</div>}
    </div>
  );
}

/** A card's list: an empty state without items, otherwise one divided row (an <li> with a key) per item. */
export function Rows<T>(props: { items: T[]; empty: ComponentChildren; emptyTitle?: ComponentChildren; class?: string; children: (x: T) => ComponentChildren }) {
  if (!props.items.length) {
    return (
      <div class="p-5">
        <Empty title={props.emptyTitle}>{props.empty}</Empty>
      </div>
    );
  }
  return <ul class={`divide-y divide-line ${props.class ?? ""}`}>{props.items.map((x) => props.children(x))}</ul>;
}

export function ErrorBox(props: { error: unknown }) {
  if (!props.error) return null;
  const text = typeof props.error === "string" ? props.error : errText(props.error);
  return (
    <div role="alert" class="rounded-lg border border-bad/40 bg-bad/10 px-3 py-2 text-bad">
      {text}
    </div>
  );
}

export function Note(props: { tone?: "info" | "warn" | "bad" | "ok"; children: ComponentChildren; class?: string }) {
  const tone = {
    info: "border-info/30 bg-info/10",
    warn: "border-warn/40 bg-warn/10",
    bad: "border-bad/40 bg-bad/10",
    ok: "border-ok/30 bg-ok/10",
  }[props.tone ?? "info"];
  return <div class={`rounded-lg border px-3 py-2 text-sm ${tone} ${props.class ?? ""}`}>{props.children}</div>;
}

// ---- Skeletons: the shape of what is loading, so the page does not jump when it arrives ----

/** One gray placeholder block; `class` gives it its size. */
export const Skel = (props: { class?: string }) => <span aria-hidden="true" class={`block animate-pulse rounded-md bg-raised ${props.class ?? "h-4 w-full"}`} />;

/** A few lines of text, for blocks inside a padded card. */
export const Loading = () => (
  <div class="space-y-3 py-1" role="status" aria-label="Loading">
    <Skel class="h-4 w-2/3" />
    <Skel class="h-4 w-full" />
    <Skel class="h-4 w-1/2" />
  </div>
);

/** List rows (icon, two lines, a status), for cards without padding. */
export const RowsSkeleton = (props: { n?: number }) => (
  <ul role="status" aria-label="Loading" class="divide-y divide-line">
    {Array.from({ length: props.n ?? 3 }, (_, i) => (
      <li key={i} class="flex items-center gap-3 px-4 py-3.5 md:px-5">
        <Skel class="size-8 shrink-0 rounded-lg" />
        <span class="min-w-0 flex-1 space-y-2">
          <Skel class="h-3.5 w-1/3" />
          <Skel class="h-3 w-2/3" />
        </span>
        <Skel class="h-5 w-16 rounded-full" />
      </li>
    ))}
  </ul>
);

/** A card's outline with a title bar, filled with `children` (skeleton lines by default). */
export const CardSkeleton = (props: { children?: ComponentChildren; class?: string }) => (
  <div class={`card ${props.class ?? ""}`} aria-hidden="true">
    <div class="flex min-h-13 items-center border-b border-line px-4 md:px-5">
      <Skel class="h-4 w-28" />
    </div>
    {props.children ?? (
      <div class="p-4 md:p-5">
        <Loading />
      </div>
    )}
  </div>
);

/** Shows the right state for a useApi result: an error, a skeleton or the content. */
export function Loaded<T>(props: { r: { data?: T; error?: ApiError }; children: (d: T) => ComponentChildren; skeleton?: ComponentChildren }) {
  if (props.r.data !== undefined) return <>{props.children(props.r.data)}</>;
  if (props.r.error) return <ErrorBox error={props.r.error} />;
  return <>{props.skeleton ?? <Loading />}</>;
}

/** A detail page before its data arrives (`skeleton` is its shape), or with the error when it does not. */
export const LoadingPage = (props: { title?: string; crumbs?: [string, string][]; r: { data?: unknown; error?: ApiError }; skeleton?: ComponentChildren }) => (
  <Page title={props.title} crumbs={props.crumbs}>
    <Loaded r={props.r} skeleton={props.skeleton}>
      {() => null}
    </Loaded>
  </Page>
);

const TONE: Record<string, string> = {
  running: "ok",
  live: "ok",
  healthy: "ok",
  degraded: "warn",
  queued: "warn",
  building: "warn",
  deploying: "warn",
  starting: "warn",
  restarting: "bad",
  unhealthy: "bad",
  crashing: "bad",
  failed: "bad",
  dead: "bad",
  asleep: "info",
  following: "ok",
  copying: "warn",
  online: "ok",
  joining: "warn",
  joined: "ok",
  creating: "warn",
  unreachable: "warn",
  offline: "bad",
};
const TONE_CLASS: Record<string, string> = {
  ok: "border-ok/30 bg-ok/10 text-ok",
  warn: "border-warn/40 bg-warn/10 text-warn",
  bad: "border-bad/40 bg-bad/10 text-bad",
  info: "border-info/30 bg-info/10 text-info",
  muted: "border-line bg-raised text-dim",
};

/** A state label with a colored dot; the text carries the meaning, the color only reinforces it. */
export function Pill(props: { state: string; label?: string; class?: string }) {
  const tone = TONE[props.state] ?? "muted";
  return (
    <span class={`inline-flex items-center gap-1.5 whitespace-nowrap rounded-full border px-2 py-px text-xs font-medium ${TONE_CLASS[tone]} ${props.class ?? ""}`}>
      <span class="size-1.5 rounded-full bg-current" aria-hidden="true" />
      {props.label ?? props.state}
    </span>
  );
}

/** Just the colored dot of a state, for dense lists; `title` carries the state's name. */
export function StateDot(props: { state: string }) {
  const tone = TONE[props.state] ?? "muted";
  const bg = { ok: "bg-ok", warn: "bg-warn", bad: "bg-bad", info: "bg-info" }[tone] ?? "bg-muted";
  return <span class={`size-2 shrink-0 rounded-full ${bg}`} title={props.state} aria-label={props.state} role="img" />;
}

/** Page tabs. On phones they scroll sideways, start scrolled to the current one and fade at the edges. */
export function Tabs(props: { items: [string, string, string][]; current: string; label: string }) {
  const ref = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    const nav = ref.current;
    const tab = nav?.querySelector<HTMLElement>("[aria-current]");
    if (!nav || !tab) return;
    const n = nav.getBoundingClientRect();
    const t = tab.getBoundingClientRect();
    nav.scrollLeft += t.left - n.left - (n.width - t.width) / 2;
  }, [props.current]);
  return (
    <nav
      ref={ref}
      aria-label={props.label}
      class="-mx-4 mb-7 overflow-x-auto border-b border-line px-4 [scrollbar-width:none] max-md:[mask-image:linear-gradient(to_right,transparent,#000_1rem,#000_calc(100%-1rem),transparent)] md:mx-0 md:px-0"
    >
      <div class="flex gap-0.5">
        {props.items.map(([id, label, href]) => (
          <a
            key={id}
            href={href}
            aria-current={id === props.current ? "page" : undefined}
            class={`-mb-px whitespace-nowrap border-b-2 px-3 pt-2.5 pb-3 text-sm font-medium md:pt-1.5 md:pb-2.5 md:text-[13px] ${
              id === props.current ? "border-accent text-text" : "border-transparent text-muted hover:text-text"
            }`}
          >
            {label}
          </a>
        ))}
      </div>
    </nav>
  );
}

// ---- Toasts for page-level actions (forms show their errors inline instead) ----

interface Toast {
  id: number;
  text: string;
  tone: "ok" | "bad";
}
let pushToast: (t: Toast) => void = () => {};
let toastId = 0;
export const toast = (text: string, tone: "ok" | "bad" = "ok") => pushToast({ id: ++toastId, text, tone });

export function Toasts() {
  const [list, setList] = useState<Toast[]>([]);
  const ref = useRef<HTMLDivElement>(null);
  pushToast = (t) => {
    setList((l) => [...l, t]);
    setTimeout(() => setList((l) => l.filter((x) => x.id !== t.id)), t.tone === "bad" ? 7000 : 3500);
  };
  // A manual popover lives in the top layer; showing it again puts it above any open dialog.
  useEffect(() => {
    const el = ref.current;
    if (!el?.showPopover) return;
    try {
      if (el.matches(":popover-open")) el.hidePopover();
      if (list.length) el.showPopover();
    } catch {
      // Browsers without popover support still show the fixed-position stack.
    }
  }, [list]);
  return (
    <div
      ref={ref}
      popover="manual"
      aria-live="polite"
      class="pointer-events-none fixed top-auto right-4 bottom-4 left-4 z-50 m-0 flex h-auto w-auto flex-col items-end gap-2 overflow-visible border-0 bg-transparent p-0"
    >
      {list.map((t) => (
        <div
          key={t.id}
          role={t.tone === "bad" ? "alert" : "status"}
          class={`pointer-events-auto max-w-sm rounded-lg border px-3.5 py-2.5 text-sm shadow-lg ${
            t.tone === "bad" ? "border-bad/40 bg-surface text-bad" : "border-line bg-surface text-text"
          }`}
        >
          {t.text}
        </div>
      ))}
    </div>
  );
}
