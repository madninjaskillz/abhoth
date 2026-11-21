// The HTML preview's address bar and back/forward, without React or the DOM (so plain node can
// test it: test/previewNav.test.ts). The frame is sandboxed (another origin), so the app can't read
// where it is; the server adds a small script to each page that posts its location, and these
// helpers keep a history of what it reported.

/** Files the preview can show. */
export const isHtmlPath = (path: string) => /\.html?$/i.test(path);

export interface PreviewNav {
  /** Every page visited (full URLs as the frame reported them). */
  entries: string[];
  index: number;
  /** The entry back/forward asked for, until the frame reports arriving there. */
  pending: number | null;
}

export const emptyNav: PreviewNav = { entries: [], index: -1, pending: null };

/** The frame reported a page: arriving at the pending entry, a reload (same page), or a new page. */
export function navReport(nav: PreviewNav, href: string): PreviewNav {
  if (nav.pending !== null && nav.entries[nav.pending] === href) return { ...nav, index: nav.pending, pending: null };
  if (nav.entries[nav.index] === href) return { ...nav, pending: null };
  const entries = [...nav.entries.slice(0, nav.index + 1), href];
  return { entries, index: entries.length - 1, pending: null };
}

export const canBack = (nav: PreviewNav) => nav.index > 0;
export const canForward = (nav: PreviewNav) => nav.index >= 0 && nav.index < nav.entries.length - 1;

/** Back (-1) or forward (+1): the state while going there, and the URL to load; null when there's nothing that way. */
export function navStep(nav: PreviewNav, delta: -1 | 1): { nav: PreviewNav; href: string } | null {
  const i = nav.index + delta;
  if (i < 0 || i >= nav.entries.length) return null;
  return { nav: { ...nav, pending: i }, href: nav.entries[i] };
}

/** A new preview token (reload): the same pages under the new base. */
export function rebase(nav: PreviewNav, oldBase: string, newBase: string): PreviewNav {
  const swap = (href: string) => {
    const at = href.indexOf(oldBase);
    return at >= 0 ? href.slice(0, at) + newBase + href.slice(at + oldBase.length) : href;
  };
  return { ...nav, entries: nav.entries.map(swap) };
}

/**
 * What the address bar shows for a URL: the project-relative path (with its query and hash) when
 * it is under the preview's base, else the URL itself (a page elsewhere).
 */
export function displayPath(href: string, base: string, dir: string): string {
  let u: URL;
  try {
    u = new URL(href, "http://x");
  } catch {
    return href;
  }
  if (!u.pathname.startsWith(base)) return href;
  let rel = u.pathname.slice(base.length);
  try {
    rel = decodeURIComponent(rel);
  } catch {
    /* keep it encoded */
  }
  return (dir ? `${dir}/` : "") + rel + u.search + u.hash;
}

/**
 * The URL to load for what was typed in the address bar: an http(s) URL as is; a project path
 * (optionally with a leading /) under the previewed folder; null for anything else.
 */
export function resolveAddress(input: string, base: string, dir: string): string | null {
  const s = input.trim();
  if (!s) return null;
  if (/^https?:\/\//i.test(s)) return s;
  const cut = s.search(/[?#]/);
  const path = (cut >= 0 ? s.slice(0, cut) : s).replace(/\\/g, "/").replace(/^\/+/, "");
  const tail = cut >= 0 ? s.slice(cut) : "";
  let rel: string;
  if (!dir) rel = path;
  else if (path === dir) rel = "";
  else if (path.startsWith(dir + "/")) rel = path.slice(dir.length + 1);
  else return null;
  if (rel.split("/").some((p) => p === ".." || p.startsWith("."))) return null;
  return base + rel.split("/").map(encodeURIComponent).join("/") + tail;
}

/** A message from the preview frame's script, if that's what it is. */
export function previewMessage(data: unknown): { href: string; title: string } | null {
  if (!data || typeof data !== "object") return null;
  const d = data as { type?: unknown; href?: unknown; title?: unknown };
  if (d.type !== "openorca-preview" || typeof d.href !== "string" || d.href.length > 8192) return null;
  return { href: d.href, title: typeof d.title === "string" ? d.title.slice(0, 200) : "" };
}
