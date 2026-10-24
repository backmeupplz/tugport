import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";
import { ApiError, post } from "../api";
import { CodeInput, ErrorBox, Field, Input, useForm, useSubmit } from "../ui";

function AuthCard(props: { title: string; sub?: string; children: ComponentChildren }) {
  return (
    <main class="flex min-h-dvh items-center justify-center px-4 py-10">
      <div class="w-full max-w-sm">
        <div class="mb-7 flex flex-col items-center gap-3">
          <img src="/logo.svg" alt="" class="size-11" />
          <span class="text-lg font-semibold tracking-tight">Tugport</span>
        </div>
        <div class="card p-7">
          <h1 class="text-lg font-semibold tracking-tight">{props.title}</h1>
          {props.sub && <p class="mt-1 text-dim">{props.sub}</p>}
          <div class="mt-5">{props.children}</div>
        </div>
      </div>
    </main>
  );
}

export function Setup(props: { onDone: () => void }) {
  const [token, setToken] = useState(location.hash.slice(1));
  useEffect(() => {
    const read = () => setToken(location.hash.slice(1));
    window.addEventListener("hashchange", read);
    return () => window.removeEventListener("hashchange", read);
  }, []);
  const { f, bind } = useForm({ name: "", email: "", password: "" });
  const s = useSubmit(async () => {
    await post("/setup", { token, name: f.name.trim(), email: f.email.trim() || undefined, password: f.password });
    history.replaceState(null, "", "/");
    props.onDone();
  });
  if (!token) {
    return (
      <AuthCard title="Finish setting up Tugport">
        <p class="text-dim">
          Open the setup link that Tugport printed when it started. If you lost it, run <code>tugport setup-link</code> on the server to print a new one.
        </p>
      </AuthCard>
    );
  }
  return (
    <AuthCard title="Create the owner account" sub="The owner can do everything, including adding people and agents with narrower access.">
      <form class="space-y-4" onSubmit={s.submit}>
        <Input label="Name" required autoFocus autoComplete="username" {...bind("name")} />
        <Input label="Email (optional)" type="email" autoComplete="email" {...bind("email")} />
        <Input label="Password" hint="Use at least 10 characters." type="password" required minLength={10} autoComplete="new-password" {...bind("password")} />
        <ErrorBox error={s.error} />
        <button class="btn btn-primary w-full" disabled={s.busy}>
          Create account
        </button>
      </form>
    </AuthCard>
  );
}

export function Login(props: { onDone: () => void }) {
  const { f, bind, set } = useForm({ name: "", password: "", totp: "" });
  const [needCode, setNeedCode] = useState(false);
  const s = useSubmit(async () => {
    try {
      await post("/auth/login", { name: f.name.trim(), password: f.password, totp: needCode ? f.totp.trim() : undefined });
    } catch (e) {
      if (e instanceof ApiError && e.message === "totp_required") {
        setNeedCode(true);
        return;
      }
      throw e;
    }
    props.onDone();
  });
  return (
    <AuthCard title={needCode ? "Two-factor code" : "Sign in"} sub={needCode ? "Enter the 6-digit code from your authenticator app." : undefined}>
      <form class="space-y-4" onSubmit={s.submit}>
        {needCode ? (
          <Field label="Code">
            <CodeInput autoFocus {...bind("totp")} />
          </Field>
        ) : (
          <>
            <Input label="Name or email" required autoFocus autoComplete="username" {...bind("name")} />
            <Input label="Password" type="password" required autoComplete="current-password" {...bind("password")} />
          </>
        )}
        <ErrorBox error={s.error} />
        <button class="btn btn-primary w-full" disabled={s.busy}>
          {needCode ? "Verify" : "Sign in"}
        </button>
        {needCode && (
          <button
            type="button"
            class="btn btn-ghost w-full"
            onClick={() => {
              setNeedCode(false);
              set({ totp: "" });
              s.setError("");
            }}
          >
            Use a different account
          </button>
        )}
      </form>
    </AuthCard>
  );
}
