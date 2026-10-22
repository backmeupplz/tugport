// Key/value editor for project variables and service environment. Values are hidden by
// default; without `secrets:read` the API returns names only and the editor is write-only.
import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { put, useApi } from "./api";
import { Check, ErrorBox, Loaded, Note, useSubmit } from "./ui";
import { parseEnv, validEnvKey } from "./util";

export function EnvEditor(props: {
  path: string;
  field: "vars" | "env";
  canEdit: boolean;
  savedNote: ComponentChildren;
  footer?: (data: any) => ComponentChildren;
}) {
  const r = useApi<any>(props.path);
  const [changes, setChanges] = useState<Record<string, string | null>>({});
  const [show, setShow] = useState(false);
  const [draft, setDraft] = useState({ key: "", value: "" });
  const [paste, setPaste] = useState("");
  const [saved, setSaved] = useState(false);
  const [addError, setAddError] = useState("");
  const s = useSubmit(async () => {
    await put(props.path, changes);
    setChanges({});
    setSaved(true);
    await r.reload();
  });

  return (
    <Loaded r={r}>
      {(data) => {
        const raw = data[props.field];
        const values: Record<string, string> | null = data.values_visible && !Array.isArray(raw) ? raw : null;
        const names: string[] = (Array.isArray(raw) ? raw : Object.keys(raw ?? {})).slice().sort();
        const added = Object.keys(changes).filter((k) => !names.includes(k));
        const pending = Object.keys(changes).length;
        const change = (k: string, v: string | null | undefined) =>
          setChanges((c) => {
            const n = { ...c };
            if (v === undefined) delete n[k];
            else n[k] = v;
            return n;
          });
        const addMany = (map: Record<string, string>) => {
          const bad = Object.keys(map).filter((k) => !validEnvKey(k));
          if (bad.length) {
            setAddError(`${bad.join(", ")} ${bad.length > 1 ? "are not valid names" : "is not a valid name"}; use letters, digits and underscores, not starting with a digit.`);
            return false;
          }
          setAddError("");
          setSaved(false);
          setChanges((c) => ({ ...c, ...map }));
          return true;
        };
        const placeholder = values ? "Hidden; type to replace it" : "Write-only; type a new value";

        return (
          <div class="space-y-4">
            {!data.values_visible && (
              <p class="text-xs text-muted">
                Values stay hidden because reading them needs the secrets:read grant. You can still replace or delete them.
              </p>
            )}
            {names.length === 0 && added.length === 0 ? (
              <p class="text-muted">There are no variables yet.</p>
            ) : (
              <ul class="space-y-2">
                {names.map((k) => {
                  const removed = changes[k] === null;
                  const shown = show && values;
                  return (
                    <li key={k} class="flex flex-wrap items-center gap-2 sm:flex-nowrap">
                      <code class={`w-full truncate sm:w-56 sm:shrink-0 ${removed ? "text-muted line-through" : ""}`} title={k}>
                        {k}
                      </code>
                      <input
                        class="input min-w-0 flex-1 font-mono"
                        aria-label={`Value of ${k}`}
                        disabled={!props.canEdit || removed}
                        placeholder={removed ? "Will be deleted" : placeholder}
                        autoComplete="off"
                        spellcheck={false}
                        value={shown ? (changes[k] ?? values[k] ?? "") : (changes[k] ?? "")}
                        onInput={(e) => {
                          const v = e.currentTarget.value;
                          setSaved(false);
                          change(k, shown ? (v === values[k] ? undefined : v) : v === "" ? undefined : v);
                        }}
                      />
                      {props.canEdit && (
                        <button type="button" class="btn btn-ghost px-2 text-xs" onClick={() => change(k, removed ? undefined : null)}>
                          {removed ? "Undo" : "Delete"}
                        </button>
                      )}
                    </li>
                  );
                })}
                {added.map((k) => (
                  <li key={k} class="flex flex-wrap items-center gap-2 sm:flex-nowrap">
                    <code class="w-full truncate text-ok sm:w-56 sm:shrink-0">+ {k}</code>
                    <input
                      class="input min-w-0 flex-1 font-mono"
                      aria-label={`Value of ${k}`}
                      value={changes[k] ?? ""}
                      onInput={(e) => change(k, e.currentTarget.value)}
                    />
                    <button type="button" class="btn btn-ghost px-2 text-xs" onClick={() => change(k, undefined)}>
                      Remove
                    </button>
                  </li>
                ))}
              </ul>
            )}
            {values && names.length > 0 && <Check checked={show} onChange={setShow} label="Show values" />}

            {props.canEdit && (
              <>
                <form
                  class="flex flex-wrap items-end gap-2 border-t border-line pt-4 sm:flex-nowrap"
                  onSubmit={(e) => {
                    e.preventDefault();
                    if (draft.key && addMany({ [draft.key.trim()]: draft.value })) setDraft({ key: "", value: "" });
                  }}
                >
                  <input
                    class="input font-mono sm:w-56 sm:shrink-0"
                    aria-label="New variable name"
                    placeholder="NAME"
                    autoComplete="off"
                    spellcheck={false}
                    value={draft.key}
                    onInput={(e) => setDraft({ ...draft, key: e.currentTarget.value })}
                  />
                  <input
                    class="input min-w-0 flex-1 font-mono"
                    aria-label="New variable value"
                    placeholder="value"
                    autoComplete="off"
                    spellcheck={false}
                    value={draft.value}
                    onInput={(e) => setDraft({ ...draft, value: e.currentTarget.value })}
                  />
                  <button class="btn" disabled={!draft.key.trim()}>
                    Add
                  </button>
                </form>
                <details>
                  <summary class="text-sm text-dim">Paste a .env file</summary>
                  <div class="mt-2 space-y-2">
                    <textarea
                      class="input h-28"
                      aria-label=".env contents"
                      placeholder={"DATABASE_URL=postgres://…\nAPI_KEY=…"}
                      spellcheck={false}
                      value={paste}
                      onInput={(e) => setPaste(e.currentTarget.value)}
                    />
                    <button
                      type="button"
                      class="btn"
                      disabled={!paste.trim()}
                      onClick={() => addMany(parseEnv(paste)) && setPaste("")}
                    >
                      Add these variables
                    </button>
                  </div>
                </details>
                <ErrorBox error={addError} />
                <ErrorBox error={s.error} />
                {pending > 0 && (
                  <div class="flex flex-wrap items-center justify-between gap-2 rounded-md border border-accent/40 bg-accent/10 px-3 py-2">
                    <span>
                      {pending} unsaved {pending === 1 ? "change" : "changes"}
                    </span>
                    <span class="flex gap-2">
                      <button type="button" class="btn" onClick={() => setChanges({})}>
                        Discard
                      </button>
                      <button type="button" class="btn btn-primary" disabled={s.busy} onClick={() => s.submit()}>
                        Save
                      </button>
                    </span>
                  </div>
                )}
                {saved && pending === 0 && <Note tone="ok">{props.savedNote}</Note>}
              </>
            )}
            {props.footer?.(data)}
          </div>
        );
      }}
    </Loaded>
  );
}
