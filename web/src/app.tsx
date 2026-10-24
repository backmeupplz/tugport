import type { ComponentChildren, ComponentType } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { ApiError, MeContext, canSeeServers, clearCache, del, get, onUnauthorized, post, put, startEvents, stopEvents, useNodes, type Me } from "./api";
import { useUrl } from "./router";
import { Icon, type IconName } from "./icons";
import { CodeBlock, CodeInput, Empty, ErrorBox, Field, Modal, Page, Toasts, toast, useDialog, useStoredChoice, useSubmit } from "./ui";
import { copyText, matchPath } from "./util";
import { Access, Audit, NewAgent, NewPerson, UserPage } from "./pages/access";
import { Actions } from "./pages/actions";
import { Login, Setup } from "./pages/auth";
import { CloudPage } from "./pages/cloud";
import { ProjectPage, Projects } from "./pages/projects";
import { ServerPage, Servers } from "./pages/servers";
import { ServicePage } from "./pages/service";
import { Settings } from "./pages/settings";

type Gate = "loading" | "setup" | "login" | "in" | "down";

/** The pages by path; the first pattern that fits wins. */
const ROUTES: [string, ComponentType<any>][] = [
  ["/", Projects],
  ["/projects/:project", ProjectPage],
  ["/projects/:project/:service/:tab?", ServicePage],
  ["/actions", Actions],
  ["/servers", Servers],
  ["/servers/cloud", CloudPage],
  ["/servers/:id", ServerPage],
  ["/server", ServerPage],
  ["/access", Access],
  ["/access/audit", Audit],
  ["/access/new-agent", NewAgent],
  ["/access/new-person", NewPerson],
  ["/access/users/:id", UserPage],
  ["/settings/:tab?", Settings],
];

function Routes(props: { path: string }) {
  for (const [pattern, View] of ROUTES) {
    const params = matchPath(pattern, props.path);
    if (params) return <View {...params} />;
  }
  return <NotFound />;
}

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
          clearCache();
          setMe(null);
          setGate("login");
        }}
      />
    </MeContext.Provider>
  );
}

type Theme = "system" | "light" | "dark";

function useTheme(): [Theme, (t: Theme) => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    try {
      return (localStorage.getItem("tugport-theme") as Theme | null) ?? "system";
    } catch {
      return "system";
    }
  });
  const choose = (next: Theme) => {
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
  return [theme, choose];
}

const THEMES: [Theme, string, IconName][] = [
  ["system", "System", "monitor"],
  ["light", "Light", "sun"],
  ["dark", "Dark", "moon"],
];

/** Whether the desktop sidebar is folded to icons, remembered per browser. */
function useCollapsed(): [boolean, () => void] {
  const [state, set] = useStoredChoice<"open" | "collapsed">("tugport-sidebar", "open");
  return [state === "collapsed", () => set(state === "collapsed" ? "open" : "collapsed")];
}

/** The public address of the panel's server, copied with a tap (people who see servers only). */
function ServerAddress(props: { compact: boolean }) {
  const address = useNodes()?.nodes.find((n) => n.local)?.address;
  if (!address) return null;
  const copy = async () => toast((await copyText(address)) ? `Copied ${address}.` : "Copying did not work; select the address instead.", "ok");
  return props.compact ? (
    <button type="button" class="mx-auto grid size-9 place-items-center rounded-lg text-dim hover:bg-raised hover:text-text" title={`Server address ${address}, click to copy`} aria-label={`Copy the server address ${address}`} onClick={copy}>
      <Icon name="copy" />
    </button>
  ) : (
    <button type="button" class="group mx-3 flex w-[calc(100%-1.5rem)] items-center gap-2 rounded-lg px-3 py-2 text-left hover:bg-raised" title="Click to copy" onClick={copy}>
      <span class="min-w-0 flex-1">
        <span class="block text-xs text-muted">Server address</span>
        <span class="block truncate font-mono text-[13px]">{address}</span>
      </span>
      <Icon name="copy" class="size-4 text-muted group-hover:text-text" />
    </button>
  );
}

function Shell(props: { me: Me; reloadMe: () => Promise<void>; signOut: () => void }) {
  const { me } = props;
  const url = useUrl();
  const [drawer, setDrawer] = useState(false);
  const [collapsed, toggleCollapsed] = useCollapsed();
  const [twofa, setTwofa] = useState(false);
  const [theme, setTheme] = useTheme();
  const path = url.split("?")[0];
  // A new page starts at the top, before it paints; the drawer closes on any navigation.
  const shown = useRef(path);
  useLayoutEffect(() => {
    if (shown.current !== path) window.scrollTo(0, 0);
    shown.current = path;
    setDrawer(false);
  }, [url]);
  const nav: [string, string, IconName, boolean][] = [
    ["Projects", "/", "projects", path === "/" || path.startsWith("/projects")],
    ["Actions", "/actions", "actions", path.startsWith("/actions")],
    ...(canSeeServers(me) ? [["Servers", "/servers", "servers", path.startsWith("/server")] as [string, string, IconName, boolean]] : []),
    ["Access", "/access", "access", path.startsWith("/access")],
    ["Settings", "/settings", "settings", path.startsWith("/settings")],
  ];
  // `compact`: the folded desktop sidebar, icons only.
  const sidebar = (compact: boolean) => (
    <div class="flex h-full flex-col">
      <div class={`flex items-center gap-2.5 pt-5 pb-5 ${compact ? "flex-col px-2" : "px-5"}`}>
        <a href="/" class="flex min-w-0 flex-1 items-center gap-2.5" aria-label="Tugport">
          <img src="/logo.svg" alt="" class="size-7" />
          {!compact && <span class="text-[15px] font-semibold tracking-tight">Tugport</span>}
        </a>
        <button
          type="button"
          class="hidden size-8 place-items-center rounded-lg text-muted hover:bg-raised hover:text-text md:grid"
          aria-label={compact ? "Expand the sidebar" : "Collapse the sidebar"}
          title={compact ? "Expand the sidebar" : "Collapse the sidebar"}
          onClick={toggleCollapsed}
        >
          <Icon name="sidebar" />
        </button>
      </div>
      <nav aria-label="Main" class={`flex-1 ${compact ? "px-2" : "px-3"}`}>
        <ul class="space-y-0.5">
          {nav.map(([label, href, icon, active]) => (
            <li key={href}>
              <a
                href={href}
                aria-current={active ? "page" : undefined}
                aria-label={compact ? label : undefined}
                title={compact ? label : undefined}
                class={`flex h-11 items-center gap-3 rounded-lg text-[15px] font-medium md:h-9 md:text-sm ${compact ? "justify-center" : "px-3"} ${active ? "bg-raised text-text" : "text-dim hover:bg-raised/60 hover:text-text"}`}
              >
                <Icon name={icon} class={`size-4 ${active ? "text-accent" : ""}`} />
                {!compact && label}
              </a>
            </li>
          ))}
        </ul>
      </nav>
      <div class="space-y-0.5 pb-2">
        <ServerAddress compact={compact} />
        <a
          href="https://github.com/backmeupplz/tugport/issues"
          target="_blank"
          rel="noreferrer"
          title={compact ? "Support: report a problem or ask on GitHub" : undefined}
          aria-label={compact ? "Support" : undefined}
          class={`flex h-9 items-center gap-3 rounded-lg text-sm text-dim hover:bg-raised hover:text-text ${compact ? "mx-auto w-9 justify-center" : "mx-3 px-3"}`}
        >
          <Icon name="help" />
          {!compact && (
            <>
              <span class="flex-1">Support</span>
              <Icon name="external" class="size-3.5 text-muted" />
            </>
          )}
        </a>
      </div>
      <Account me={me} compact={compact} theme={theme} setTheme={setTheme} openTwofa={() => setTwofa(true)} signOut={props.signOut} />
    </div>
  );

  return (
    <div class="min-h-dvh md:flex">
      <a href="#main" class="sr-only focus:not-sr-only focus:fixed focus:left-2 focus:top-2 focus:z-50 focus:rounded focus:bg-surface focus:px-3 focus:py-2">
        Skip to content
      </a>
      <aside class={`hidden shrink-0 border-r border-line md:sticky md:top-0 md:block md:h-dvh md:overflow-y-auto ${collapsed ? "w-16" : "w-60"}`}>{sidebar(collapsed)}</aside>
      <Drawer open={drawer} onClose={() => setDrawer(false)}>
        {sidebar(false)}
      </Drawer>
      <div class="min-w-0 flex-1">
        <header class="sticky top-0 z-30 flex items-center justify-between border-b border-line bg-bg/85 px-4 py-2.5 backdrop-blur md:hidden">
          <a href="/" class="flex items-center gap-2">
            <img src="/logo.svg" alt="" class="size-7" />
            <span class="font-semibold tracking-tight">Tugport</span>
          </a>
          <button class="btn btn-ghost px-2" aria-label="Open the menu" aria-expanded={drawer} onClick={() => setDrawer(true)}>
            <Icon name="menu" class="size-5" />
          </button>
        </header>
        <main id="main">
          <Routes path={path} />
        </main>
      </div>
      <TwoFactor open={twofa} onClose={() => setTwofa(false)} me={me} reloadMe={props.reloadMe} />
      <Toasts />
    </div>
  );
}

/** The mobile navigation, as a modal dialog so it traps focus and closes with Escape. */
function Drawer(props: { open: boolean; onClose: () => void; children: ComponentChildren }) {
  const ref = useDialog(props.open);
  return (
    <dialog
      ref={ref}
      onClose={props.onClose}
      onClick={(e) => e.target === ref.current && props.onClose()}
      aria-label="Navigation"
      class="m-0 h-dvh max-h-none w-72 max-w-[85vw] border-r border-line bg-bg p-0 text-text md:hidden"
    >
      {props.children}
      <button type="button" class="btn btn-ghost absolute top-3.5 right-3 px-2.5" aria-label="Close the menu" onClick={props.onClose}>
        <Icon name="close" class="size-5" />
      </button>
    </dialog>
  );
}

/**
 * Who is signed in, with a menu (a popover, so it closes on Escape or an outside click) for
 * two-factor sign-in, the theme and signing out.
 */
function Account(props: { me: Me; compact: boolean; theme: Theme; setTheme: (t: Theme) => void; openTwofa: () => void; signOut: () => void }) {
  const { principal } = props.me;
  const menu = useRef<HTMLDivElement>(null);
  const item = "flex h-9 w-full items-center gap-2.5 rounded-lg px-2.5 text-left hover:bg-raised";
  const run = (f: () => void) => () => {
    menu.current?.hidePopover?.();
    f();
  };
  return (
    <div class={`border-t border-line ${props.compact ? "p-2" : "p-3"}`}>
      <button
        type="button"
        class={`flex w-full items-center gap-2.5 rounded-lg p-2 text-left hover:bg-raised ${props.compact ? "justify-center" : ""}`}
        aria-label={`Account: ${principal.name}`}
        title={props.compact ? principal.name : undefined}
        onClick={() => menu.current?.togglePopover?.()}
      >
        <span class="grid size-8 shrink-0 place-items-center rounded-full bg-accent text-sm font-semibold text-accent-ink" aria-hidden="true">
          {principal.name.slice(0, 1).toUpperCase()}
        </span>
        {!props.compact && (
          <>
            <span class="min-w-0 flex-1">
              <span class="block truncate font-medium">{principal.name}</span>
              <span class="block text-xs text-muted">{principal.is_owner ? "Owner" : principal.kind === "agent" ? "Agent" : "Member"}</span>
            </span>
            <Icon name="updown" class="size-4 text-muted" />
          </>
        )}
      </button>
      <div
        ref={menu}
        popover="auto"
        aria-label="Account"
        class="fixed top-auto right-auto bottom-[4.5rem] left-3 m-0 w-56 rounded-xl border border-line bg-surface p-1.5 text-text shadow-2xl"
      >
        {principal.kind === "human" && (
          <button type="button" class={item} onClick={run(props.openTwofa)}>
            <Icon name="shield" class="size-4 text-dim" />
            <span class="flex-1">Two-factor sign-in</span>
            <span class="text-xs text-muted">{props.me.totp ? "On" : "Off"}</span>
          </button>
        )}
        <div role="group" aria-label="Theme" class="my-1 grid grid-cols-3 gap-1 rounded-lg bg-raised p-1">
          {THEMES.map(([t, label, icon]) => (
            <button
              key={t}
              type="button"
              aria-pressed={props.theme === t}
              title={label}
              class={`flex h-7 items-center justify-center gap-1 rounded-md text-xs ${props.theme === t ? "bg-surface text-text shadow-(--shadow)" : "text-dim hover:text-text"}`}
              onClick={() => props.setTheme(t)}
            >
              <Icon name={icon} class="size-3.5" />
              {label}
            </button>
          ))}
        </div>
        <button type="button" class={item} onClick={run(props.signOut)}>
          <Icon name="signout" class="size-4 text-dim" />
          Sign out
        </button>
      </div>
    </div>
  );
}

function NotFound() {
  return (
    <Page title="Not found">
      <Empty
        action={
          <a class="btn" href="/">
            Go to projects
          </a>
        }
      >
        This page does not exist, or you do not have access to it.
      </Empty>
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
      <CodeInput value={code} onInput={(e) => setCode(e.currentTarget.value)} />
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
