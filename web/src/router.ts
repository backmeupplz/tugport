// Client-side navigation: route() and clicks on links to the panel's own pages push a history
// entry instead of loading the page again, and useUrl() follows them and the back button.
import { useEffect, useState } from "preact/hooks";

const followers = new Set<() => void>();

export const currentUrl = () => location.pathname + location.search;

export function route(url: string) {
  history.pushState(null, "", url);
  followers.forEach((f) => f());
}

/** Plain left clicks on same-tab links to a path ("/x", not "//host") stay in the app. */
function onClick(e: MouseEvent) {
  if (e.defaultPrevented || e.button || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const a = (e.target as Element | null)?.closest?.("a");
  const href = a?.getAttribute("href");
  if (!a || !href || !/^\/(?!\/)/.test(href) || (a.target && a.target !== "_self") || a.hasAttribute("download")) return;
  e.preventDefault();
  route(href);
}

/** The current path and query, updated on every navigation. */
export function useUrl() {
  const [url, setUrl] = useState(currentUrl);
  useEffect(() => {
    const sync = () => setUrl(currentUrl());
    followers.add(sync);
    addEventListener("popstate", sync);
    addEventListener("click", onClick);
    return () => {
      followers.delete(sync);
      removeEventListener("popstate", sync);
      removeEventListener("click", onClick);
    };
  }, []);
  return url;
}
