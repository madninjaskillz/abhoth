import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowLeft, ArrowRight, Code, ExternalLink, Loader2, RotateCw } from "lucide-react";
import { projectApi } from "@/lib/api";
import { Tip } from "@/components/ui/misc";
import {
  canBack,
  canForward,
  displayPath,
  emptyNav,
  navReport,
  navStep,
  previewMessage,
  rebase,
  resolveAddress,
  type PreviewNav,
} from "./previewNav";

/**
 * An HTML file shown as a page, in a sandboxed frame (scripts, forms, pop-ups and dialogs, but
 * another origin: the page can't reach the app's cookies, storage or API), with back, forward,
 * reload, an address bar, open in a new tab and a switch to the source. The server serves the
 * file's folder under a signed, expiring URL, so relative links and assets work.
 */
export function HtmlPreview({ pid, wt, path, onSource }: { pid: string; wt: string; path: string; onSource: () => void }) {
  const frame = useRef<HTMLIFrameElement | null>(null);
  const [grant, setGrant] = useState<{ url: string; base: string; dir: string } | null>(null);
  const [src, setSrc] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [nav, setNav] = useState<PreviewNav>(emptyNav);
  const [address, setAddress] = useState("");
  const [editing, setEditing] = useState(false);
  const navRef = useRef(nav);
  navRef.current = nav;
  const grantRef = useRef(grant);
  grantRef.current = grant;

  const mint = useCallback(() => projectApi(pid, wt).preview(path), [pid, wt, path]);

  // A fresh link whenever the file changes (new tab, other file).
  useEffect(() => {
    let live = true;
    setError(null);
    setNav(emptyNav);
    mint()
      .then((g) => {
        if (!live) return;
        setGrant(g);
        setSrc(g.url);
        setLoading(true);
      })
      .catch((e) => live && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      live = false;
    };
  }, [mint]);

  const go = useCallback((href: string) => {
    const w = frame.current?.contentWindow;
    setLoading(true);
    try {
      // A parent may navigate its own frame whatever the frame's origin; replace() keeps the
      // browser's history clean (this bar keeps its own).
      if (w) w.location.replace(href);
      else setSrc(href);
    } catch {
      setSrc(href);
    }
  }, []);

  // Where the page is, from the script the server adds to each page.
  useEffect(() => {
    const onMessage = (e: MessageEvent) => {
      if (!frame.current || e.source !== frame.current.contentWindow) return;
      const m = previewMessage(e.data);
      if (!m) return;
      setNav((n) => navReport(n, m.href));
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, []);

  const current = nav.entries[nav.index] ?? (src ? new URL(src, location.href).href : "");
  useEffect(() => {
    if (!editing && grant) setAddress(current ? displayPath(current, grant.base, grant.dir) : "");
  }, [current, grant, editing]);

  // Reload: a new link (the old one may have expired), at the page being shown.
  const reload = useCallback(async () => {
    const old = grantRef.current;
    try {
      const g = await mint();
      setError(null);
      const n = old ? rebase(navRef.current, old.base, g.base) : navRef.current;
      setNav(n);
      setGrant(g);
      const here = n.entries[n.index];
      go(here && here.includes(g.base) ? here : g.url);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [mint, go]);

  // The agent (or the editor) changed a file in the page's folder: show the new version.
  useEffect(() => {
    const dir = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
    let timer: number | undefined;
    const onFiles = (e: Event) => {
      const d = (e as CustomEvent<{ pid: string; dirs: string[] | null }>).detail;
      if (d.pid !== pid || (d.dirs && !d.dirs.some((x) => x === dir || x.startsWith(dir ? dir + "/" : "")))) return;
      window.clearTimeout(timer);
      timer = window.setTimeout(() => void reload(), 400);
    };
    window.addEventListener("openorca:files", onFiles);
    return () => {
      window.removeEventListener("openorca:files", onFiles);
      window.clearTimeout(timer);
    };
  }, [pid, path, reload]);

  const step = (delta: -1 | 1) => {
    const s = navStep(nav, delta);
    if (!s) return;
    setNav(s.nav);
    go(s.href);
  };

  const submit = () => {
    if (!grant) return;
    const href = resolveAddress(address, grant.base, grant.dir);
    if (!href) {
      setError(grant.dir ? `Only pages under ${grant.dir}/ can be shown here.` : "Not a page of this project.");
      return;
    }
    setError(null);
    setEditing(false);
    go(href);
  };

  const iconBtn =
    "flex size-7 shrink-0 items-center justify-center rounded text-muted-foreground hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-40 [&_svg]:size-4";

  return (
    <div className="flex h-full min-h-0 flex-col bg-background" data-testid="html-preview">
      <div className="flex h-9 shrink-0 items-center gap-0.5 border-b px-1.5">
        <Tip label="Back">
          <button className={iconBtn} disabled={!canBack(nav)} onClick={() => step(-1)} aria-label="Back" data-testid="preview-back">
            <ArrowLeft />
          </button>
        </Tip>
        <Tip label="Forward">
          <button className={iconBtn} disabled={!canForward(nav)} onClick={() => step(1)} aria-label="Forward" data-testid="preview-forward">
            <ArrowRight />
          </button>
        </Tip>
        <Tip label="Reload">
          <button className={iconBtn} onClick={() => void reload()} aria-label="Reload" data-testid="preview-reload">
            {loading && src ? <Loader2 className="animate-spin" /> : <RotateCw />}
          </button>
        </Tip>
        <form
          className="mx-1 min-w-0 flex-1"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <input
            value={address}
            onChange={(e) => setAddress(e.target.value)}
            onFocus={(e) => {
              setEditing(true);
              e.target.select();
            }}
            onBlur={() => setEditing(false)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                setEditing(false);
                (e.target as HTMLInputElement).blur();
              }
            }}
            spellCheck={false}
            aria-label="Address"
            className="h-7 w-full min-w-0 rounded-md border bg-muted/40 px-2 font-mono text-xs text-foreground outline-none focus:border-primary"
            data-testid="preview-address"
          />
        </form>
        <Tip label="Open in a new browser tab">
          <button
            className={iconBtn}
            disabled={!current}
            onClick={() => current && window.open(current, "_blank", "noopener,noreferrer")}
            aria-label="Open in a new tab"
            data-testid="preview-open"
          >
            <ExternalLink />
          </button>
        </Tip>
        <Tip label="View source">
          <button className={iconBtn} onClick={onSource} aria-label="View source" data-testid="preview-source">
            <Code />
          </button>
        </Tip>
      </div>
      {error && <div className="shrink-0 border-b bg-destructive/10 px-3 py-1 text-xs text-destructive">{error}</div>}
      <div className="relative min-h-0 flex-1 bg-white">
        {src && (
          <iframe
            ref={frame}
            src={src}
            title={`Preview of ${path}`}
            // Never allow-same-origin: the page is untrusted and the app's origin is the API's.
            sandbox="allow-scripts allow-forms allow-popups allow-modals"
            referrerPolicy="no-referrer"
            className="absolute inset-0 size-full border-0"
            onLoad={() => setLoading(false)}
            data-testid="preview-frame"
          />
        )}
        {!src && !error && (
          <div className="absolute inset-0 flex items-center justify-center bg-background">
            <Loader2 className="size-4 animate-spin text-muted-foreground" />
          </div>
        )}
      </div>
    </div>
  );
}
