// Small shared components. Native elements first: <dialog> for modals, confirm() for
// quick confirmations, <fieldset disabled> for read-only forms.
import type { ComponentChildren, JSX } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { ApiError } from "./api";
import { copyText, sentence } from "./util";

export const errText = (e: unknown) =>
  e instanceof ApiError ? sentence(e.message) : e instanceof Error ? sentence(e.message) : "Something went wrong.";

export function Page(props: { title: ComponentChildren; sub?: ComponentChildren; actions?: ComponentChildren; children?: ComponentChildren }) {
  return (
    <div class="mx-auto w-full max-w-6xl px-4 py-6 md:px-8 md:py-8">
      <header class="mb-6 flex flex-wrap items-start justify-between gap-3">
        <div class="min-w-0">
          <h1 class="break-words text-xl font-semibold">{props.title}</h1>
          {props.sub && <div class="mt-1 text-dim">{props.sub}</div>}
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
        <header class="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <h2 class="font-semibold">{props.title}</h2>
          {props.actions && <div class="flex flex-wrap gap-2">{props.actions}</div>}
        </header>
      )}
      <div class={props.pad === false ? "" : "p-4"}>{props.children}</div>
    </section>
  );
}

/** A labelled control. The label wraps the control, which associates them without ids. */
export function Field(props: { label: ComponentChildren; hint?: ComponentChildren; children: ComponentChildren; class?: string }) {
  return (
    <label class={`block min-w-0 ${props.class ?? ""}`}>
      <span class="label">{props.label}</span>
      {props.children}
      {props.hint && <span class="hint">{props.hint}</span>}
    </label>
  );
}

export function Check(props: { checked: boolean; onChange: (v: boolean) => void; label: ComponentChildren; hint?: ComponentChildren; disabled?: boolean }) {
  return (
    <label class="flex items-start gap-2.5">
      <input
        type="checkbox"
        class="mt-0.5 size-4 shrink-0"
        checked={props.checked}
        disabled={props.disabled}
        onChange={(e) => props.onChange(e.currentTarget.checked)}
      />
      <span class="min-w-0">
        <span class="font-medium">{props.label}</span>
        {props.hint && <span class="hint">{props.hint}</span>}
      </span>
    </label>
  );
}

export function Select(props: {
  value: string;
  onChange: (v: string) => void;
  options: (string | [string, string])[];
  class?: string;
  disabled?: boolean;
  "aria-label"?: string;
}) {
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

/** Form state with `bind("field")` for text inputs, selects and textareas. */
export function useForm<T extends Record<string, any>>(init: T) {
  const [f, setF] = useState(init);
  const set = (p: Partial<T>) => setF((s) => ({ ...s, ...p }));
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

/** A button that runs an API call, disables itself meanwhile and reports the outcome as a toast. */
export function Action(props: {
  run: () => Promise<unknown>;
  children: ComponentChildren;
  class?: string;
  confirm?: string;
  done?: string;
  disabled?: boolean;
  title?: string;
}) {
  const [busy, setBusy] = useState(false);
  return (
    <button
      type="button"
      class={props.class ?? "btn"}
      title={props.title}
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

export function Modal(props: { open: boolean; onClose: () => void; title: ComponentChildren; children?: ComponentChildren; wide?: boolean }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    if (props.open && !d.open) d.showModal();
    if (!props.open && d.open) d.close();
  }, [props.open]);
  return (
    <dialog
      ref={ref}
      onClose={props.onClose}
      class={`m-auto max-h-[calc(100dvh-2rem)] w-[calc(100%-2rem)] overflow-y-auto rounded-xl border border-line bg-surface p-0 text-text shadow-2xl ${props.wide ? "max-w-3xl" : "max-w-lg"}`}
    >
      {props.open && (
        <div>
          <header class="sticky top-0 z-10 flex items-center justify-between gap-3 border-b border-line bg-surface px-5 py-3">
            <h2 class="min-w-0 truncate text-base font-semibold">{props.title}</h2>
            <button type="button" class="btn btn-ghost -mr-2 px-2" aria-label="Close" onClick={props.onClose}>
              <svg viewBox="0 0 16 16" class="size-4" aria-hidden="true">
                <path d="M4 4l8 8M12 4l-8 8" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
              </svg>
            </button>
          </header>
          <div class="p-5">{props.children}</div>
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
      class={props.class ?? "btn px-2 py-1 text-xs"}
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
    <div class="min-w-0 rounded-md border border-line bg-bg">
      <div class="flex items-center justify-between gap-2 border-b border-line px-3 py-1.5 text-xs text-dim">
        <span class="min-w-0 truncate">{props.label}</span>
        <CopyButton text={props.text} />
      </div>
      <pre class="max-h-72 overflow-auto whitespace-pre-wrap break-all p-3 font-mono text-[13px] leading-snug">{props.text}</pre>
    </div>
  );
}

export function Empty(props: { title?: ComponentChildren; children?: ComponentChildren; action?: ComponentChildren }) {
  return (
    <div class="rounded-lg border border-dashed border-line px-6 py-10 text-center">
      {props.title && <p class="font-medium">{props.title}</p>}
      {props.children && <p class="mx-auto mt-1 max-w-md text-dim">{props.children}</p>}
      {props.action && <div class="mt-4">{props.action}</div>}
    </div>
  );
}

/** Calm stand-in for endpoints that docs/API.md marks as planned. */
export const Planned = () => <Empty>This arrives in a later build.</Empty>;

export function ErrorBox(props: { error: unknown }) {
  if (!props.error) return null;
  const text = typeof props.error === "string" ? props.error : errText(props.error);
  return (
    <div role="alert" class="rounded-md border border-bad/40 bg-bad/10 px-3 py-2 text-bad">
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
  return <div class={`rounded-md border px-3 py-2 text-sm ${tone} ${props.class ?? ""}`}>{props.children}</div>;
}

export const Loading = () => <p class="py-6 text-muted">Loading…</p>;

/** Shows the right state for a useApi result: an error, a loading line or the content. */
export function Loaded<T>(props: { r: { data?: T; error?: ApiError }; children: (d: T) => ComponentChildren; planned?: boolean }) {
  if (props.r.error && props.planned && props.r.error.status === 404) return <Planned />;
  if (props.r.data !== undefined) return <>{props.children(props.r.data)}</>;
  if (props.r.error) return <ErrorBox error={props.r.error} />;
  return <Loading />;
}

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

export function Tabs(props: { items: [string, string, string][]; current: string; label: string }) {
  return (
    <nav aria-label={props.label} class="-mx-4 mb-6 overflow-x-auto border-b border-line px-4 md:mx-0 md:px-0">
      <div class="flex gap-1">
        {props.items.map(([id, label, href]) => (
          <a
            key={id}
            href={href}
            aria-current={id === props.current ? "page" : undefined}
            class={`-mb-px whitespace-nowrap border-b-2 px-3 py-2 text-sm font-medium ${
              id === props.current ? "border-accent text-text" : "border-transparent text-dim hover:text-text"
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
      class="pointer-events-none fixed top-auto right-4 bottom-4 left-4 z-50 m-0 flex flex-col items-end gap-2 overflow-visible border-0 bg-transparent p-0"
    >
      {list.map((t) => (
        <div
          key={t.id}
          role={t.tone === "bad" ? "alert" : "status"}
          class={`pointer-events-auto max-w-sm rounded-md border px-3 py-2 text-sm shadow-lg ${
            t.tone === "bad" ? "border-bad/40 bg-surface text-bad" : "border-line bg-surface text-text"
          }`}
        >
          {t.text}
        </div>
      ))}
    </div>
  );
}
