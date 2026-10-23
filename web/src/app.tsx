import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import Router, { getCurrentUrl } from "preact-router";
import { ApiError, MeContext, canSeeServers, del, get, onUnauthorized, post, put, startEvents, stopEvents, type Me } from "./api";
import { CodeBlock, Empty, ErrorBox, Field, Modal, Page, Toasts, toast, useSubmit } from "./ui";
import { Access, Audit, NewAgent, NewPerson, UserPage } from "./pages/access";
import { Login, Setup } from "./pages/auth";
import { CloudPage } from "./pages/cloud";
import { ProjectPage, Projects } from "./pages/projects";
import { ServerPage, Servers } from "./pages/servers";
import { ServicePage } from "./pages/service";
import { Settings } from "./pages/settings";

type Gate = "loading" | "setup" | "login" | "in" | "down";

export function App() {
  const [me, setMe] = useState<Me | null>(null);
  const [gate, setGate] = useState<Gate>("loading");

  const load = async () => {
    try {
      setMe(await get<Me>("/me"));
      setGate("in");
    } catch (e) {
      if (!(e instanceof ApiError) || e.status !== 401) {
        setGate("down");
        return;
      }
      const s = await get<{ needs_setup: boolean }>("/setup").catch(() => ({ needs_setup: false }));
      setGate(s.needs_setup ? "setup" : "login");
    }
  };

  useEffect(() => {
    onUnauthorized(() => setGate((g) => (g === "in" ? "login" : g)));
    load();
  }, []);

  useEffect(() => {
    if (gate !== "in") return;
    startEvents();
    return stopEvents;
  }, [gate]);

  if (gate === "setup") return <Setup onDone={load} />;
  if (gate === "login") return <Login onDone={load} />;
  if (gate === "down") {
    return (
      <main class="flex min-h-dvh items-center justify-center px-4">
        <div class="max-w-sm text-center">
          <p class="mb-4 text-dim">Tugport is not reachable right now. It may be restarting, so try again in a moment.</p>
          <button class="btn" onClick={load}>
            Try again
          </button>
        </div>
      </main>
    );
  }
  if (gate === "loading" || !me) return <main class="min-h-dvh" aria-busy="true" />;
  return (
    <MeContext.Provider value={me}>
      <Shell
        me={me}
        reloadMe={load}
        signOut={async () => {
          await post("/auth/logout").catch(() => {});
          setMe(null);
          setGate("login");
        }}
      />
    </MeContext.Provider>
  );
}

function useTheme(): [string, () => void] {
  const [theme, setTheme] = useState(() => {
    try {
      return localStorage.getItem("tugport-theme") ?? "system";
    } catch {
      return "system";
    }
  });
  const cycle = () => {
    const next = theme === "system" ? "light" : theme === "light" ? "dark" : "system";
    try {
      if (next === "system") localStorage.removeItem("tugport-theme");
      else localStorage.setItem("tugport-theme", next);
    } catch {
      // Private windows may block storage; the choice then lasts until the page reloads.
    }
    if (next === "system") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = next;
    window.dispatchEvent(new Event("tugport-theme"));
    setTheme(next);
  };
  return [theme, cycle];
}

const THEME_LABEL: Record<string, string> = { system: "System", light: "Light", dark: "Dark" };

function Shell(props: { me: Me; reloadMe: () => Promise<void>; signOut: () => void }) {
  const { me } = props;
  const [url, setUrl] = useState(getCurrentUrl());
  const [drawer, setDrawer] = useState(false);
  const [twofa, setTwofa] = useState(false);
  const [theme, cycleTheme] = useTheme();
  const path = url.split("?")[0];
  const nav: [string, string, boolean][] = [
    ["Projects", "/", path === "/" || path.startsWith("/projects")],
    ...(canSeeServers(me) ? [["Servers", "/servers", path.startsWith("/server")] as [string, string, boolean]] : []),
    ["Access", "/access", path.startsWith("/access")],
    ["Settings", "/settings", path.startsWith("/settings")],
  ];
  const sidebar = (
    <div class="flex h-full flex-col">
      <a href="/" class="flex items-center gap-2.5 px-4 py-4">
        <img src="/logo.svg" alt="" class="size-8" />
        <span class="text-base font-semibold">Tugport</span>
      </a>
      <nav aria-label="Main" class="flex-1 px-2">
        <ul class="space-y-0.5">
          {nav.map(([label, href, active]) => (
            <li key={href}>
              <a
                href={href}
                aria-current={active ? "page" : undefined}
                class={`flex items-center gap-2 rounded-md px-3 py-2 font-medium ${active ? "bg-raised text-text" : "text-dim hover:bg-raised hover:text-text"}`}
              >
                <span class={`h-4 w-0.5 rounded-full ${active ? "bg-accent" : "bg-transparent"}`} aria-hidden="true" />
                {label}
              </a>
            </li>
          ))}
        </ul>
      </nav>
      <div class="border-t border-line p-3">
        <div class="mb-2 flex items-center gap-2.5 px-1">
          <span class="grid size-8 shrink-0 place-items-center rounded-full bg-accent text-sm font-semibold text-accent-ink" aria-hidden="true">
            {me.principal.name.slice(0, 1).toUpperCase()}
          </span>
          <span class="min-w-0">
            <span class="block truncate font-medium">{me.principal.name}</span>
            <span class="block text-xs text-muted">{me.principal.is_owner ? "Owner" : me.principal.kind === "agent" ? "Agent" : "Member"}</span>
          </span>
        </div>
        <div class="flex flex-col">
          {me.principal.kind === "human" && (
            <button class="btn btn-ghost justify-start" onClick={() => setTwofa(true)}>
              Two-factor: {me.totp ? "on" : "off"}
            </button>
          )}
          <button class="btn btn-ghost justify-start" onClick={cycleTheme} aria-label={`Theme: ${THEME_LABEL[theme]}. Switch theme`}>
            Theme: {THEME_LABEL[theme]}
          </button>
          <button class="btn btn-ghost justify-start" onClick={props.signOut}>
            Sign out
          </button>
        </div>
      </div>
    </div>
  );

  return (
    <div class="min-h-dvh md:flex">
      <a href="#main" class="sr-only focus:not-sr-only focus:fixed focus:left-2 focus:top-2 focus:z-50 focus:rounded focus:bg-surface focus:px-3 focus:py-2">
        Skip to content
      </a>
      <aside class="hidden w-56 shrink-0 border-r border-line bg-surface md:sticky md:top-0 md:block md:h-dvh md:overflow-y-auto">{sidebar}</aside>
      <Drawer open={drawer} onClose={() => setDrawer(false)}>
        {sidebar}
      </Drawer>
      <div class="min-w-0 flex-1">
        <header class="sticky top-0 z-30 flex items-center justify-between border-b border-line bg-surface px-4 py-2.5 md:hidden">
          <a href="/" class="flex items-center gap-2">
            <img src="/logo.svg" alt="" class="size-7" />
            <span class="font-semibold">Tugport</span>
          </a>
          <button class="btn" aria-expanded={drawer} onClick={() => setDrawer(true)}>
            Menu
          </button>
        </header>
        <main id="main">
          <Router
            onChange={(e) => {
              if (e.previous && e.previous.split("?")[0] !== e.url.split("?")[0]) window.scrollTo(0, 0);
              setUrl(e.url);
              setDrawer(false);
            }}
          >
            <Projects path="/" />
            <ProjectPage path="/projects/:project" />
            <ServicePage path="/projects/:project/:service/:tab?" />
            <Servers path="/servers" />
            <CloudPage path="/servers/cloud" />
            <ServerPage path="/servers/:id" />
            <ServerPage path="/server" />
            <Access path="/access" />
            <Audit path="/access/audit" />
            <NewAgent path="/access/new-agent" />
            <NewPerson path="/access/new-person" />
            <UserPage path="/access/users/:id" />
            <Settings path="/settings/:tab?" />
            <NotFound default />
          </Router>
        </main>
      </div>
      <TwoFactor open={twofa} onClose={() => setTwofa(false)} me={me} reloadMe={props.reloadMe} />
      <Toasts />
    </div>
  );
}

/** The mobile navigation, as a modal dialog so it traps focus and closes with Escape. */
function Drawer(props: { open: boolean; onClose: () => void; children: ComponentChildren }) {
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
      onClick={(e) => e.target === ref.current && props.onClose()}
      aria-label="Navigation"
      class="m-0 h-dvh max-h-none w-64 max-w-[85vw] border-r border-line bg-surface p-0 text-text md:hidden"
    >
      {props.children}
    </dialog>
  );
}

function NotFound(_: { default?: boolean }) {
  return (
    <Page title="Not found">
      <Empty action={<a class="btn" href="/">Go to projects</a>}>This page does not exist, or you do not have access to it.</Empty>
    </Page>
  );
}

function TwoFactor(props: { open: boolean; onClose: () => void; me: Me; reloadMe: () => Promise<void> }) {
  const [setup, setSetup] = useState<{ secret: string; svg: string } | null>(null);
  const [code, setCode] = useState("");
  const enabled = props.me.totp;
  const close = () => {
    setSetup(null);
    setCode("");
    props.onClose();
  };
  const begin = useSubmit(async () => {
    const r = await post<{ secret: string; url: string }>("/me/totp");
    const { renderSVG } = await import("uqr");
    setSetup({ secret: r.secret, svg: renderSVG(r.url, { border: 1 }) });
  });
  const confirm = useSubmit(async () => {
    if (enabled) await del("/me/totp", { code: code.trim() });
    else await put("/me/totp", { code: code.trim() });
    toast(enabled ? "Two-factor authentication is off." : "Two-factor authentication is on.");
    close();
    await props.reloadMe();
  });
  const codeField = (label: string) => (
    <Field label={label}>
      <input
        class="input text-center font-mono text-lg tracking-[0.4em]"
        required
        inputMode="numeric"
        autoComplete="one-time-code"
        pattern="[0-9]{6}"
        maxLength={6}
        value={code}
        onInput={(e) => setCode(e.currentTarget.value)}
      />
    </Field>
  );
  return (
    <Modal open={props.open} onClose={close} title="Two-factor authentication">
      {enabled ? (
        <form class="space-y-4" onSubmit={confirm.submit}>
          <p class="text-dim">Two-factor authentication is on, so signing in asks for a code from your authenticator app.</p>
          {codeField("Code to turn it off")}
          <ErrorBox error={confirm.error} />
          <button class="btn btn-danger" disabled={confirm.busy}>
            Turn off two-factor authentication
          </button>
        </form>
      ) : !setup ? (
        <div class="space-y-4">
          <p class="text-dim">Two-factor authentication asks for a code from an authenticator app every time you sign in, so a leaked password alone is not enough.</p>
          <ErrorBox error={begin.error} />
          <button class="btn btn-primary" disabled={begin.busy} onClick={() => begin.submit()}>
            Set it up
          </button>
        </div>
      ) : (
        <form class="space-y-4" onSubmit={confirm.submit}>
          <p class="text-dim">Scan this code with your authenticator app, or enter the secret by hand. Then type the code the app shows.</p>
          <div
            class="mx-auto w-48 rounded-lg bg-white p-2 [&_svg]:block [&_svg]:h-auto [&_svg]:w-full"
            role="img"
            aria-label="QR code with your two-factor secret"
            dangerouslySetInnerHTML={{ __html: setup.svg }}
          />
          <CodeBlock label="Secret" text={setup.secret} />
          {codeField("Code from the app")}
          <ErrorBox error={confirm.error} />
          <button class="btn btn-primary" disabled={confirm.busy}>
            Turn on two-factor authentication
          </button>
        </form>
      )}
    </Modal>
  );
}
