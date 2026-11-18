import { useEffect, useRef, useState } from "react";
import { create } from "zustand";
import { FileText, Image as ImageIcon } from "lucide-react";
import { rawUrl } from "@/lib/api";
import { useProject } from "@/store/session";
import { useWorkspace } from "@/store/workspace";
import { asPath, mediaKind, relativeTo, useViewer } from "./FileLink";

/** At most this many previews per message; the rest behind "show N more". */
const MAX_PREVIEWS = 4;
/** Files bigger than this aren't previewed inline (their links still work). */
const MAX_INLINE_BYTES = { image: 20 * 1024 * 1024, audio: 512 * 1024 * 1024, video: 512 * 1024 * 1024, pdf: Infinity } as const;

const PREF = "openorca.inlineMedia";

/** The viewer's "inline media" choice (on by default), kept in this browser. */
export const useInlineMedia = create<{ on: boolean; set: (on: boolean) => void }>((set) => ({
  on: (() => {
    try {
      return localStorage.getItem(PREF) !== "0";
    } catch {
      return true;
    }
  })(),
  set: (on) => {
    try {
      localStorage.setItem(PREF, on ? "1" : "0");
    } catch {
      /* private window */
    }
    set({ on });
  },
}));

// A path with a media extension, as written in prose, code or tool output.
const MEDIA_TOKEN = /(?:[A-Za-z]:[\\/]|~?[\\/]|\.{1,2}[\\/])?[\w@.+\-[\]()\\/]*\.(?:png|jpe?g|gif|webp|bmp|avif|svg|pdf|mp3|wav|ogg|oga|flac|m4a|aac|opus|mp4|m4v|webm|mov|ogv)\b/gi;

/** The media files a chat text mentions (code spans, links, bare paths), in order, without repeats. */
export function mediaPathsIn(text: string | null | undefined): string[] {
  if (!text) return [];
  const found: string[] = [];
  const add = (raw: string) => {
    const p = asPath(raw);
    if (p && mediaKind(p) && !found.includes(p)) found.push(p);
  };
  for (const m of text.matchAll(/\]\(([^)\s]+)\)/g)) {
    try {
      add(decodeURIComponent(m[1]));
    } catch {
      add(m[1]);
    }
  }
  for (const m of text.matchAll(MEDIA_TOKEN)) {
    // Skip URLs (https://host/a.png): only files are previewed.
    if (text[(m.index ?? 0) - 1] === ":") continue;
    add(m[0]);
  }
  return found;
}

/** Inline previews of media files under a message or tool card (images, audio, video, PDFs). */
export function MediaPreviews({ paths }: { paths: string[] }) {
  const on = useInlineMedia((s) => s.on);
  const [all, setAll] = useState(false);
  const media = paths.filter((p) => mediaKind(p));
  if (!on || media.length === 0) return null;
  const shown = all ? media : media.slice(0, MAX_PREVIEWS);
  return (
    <div className="flex flex-wrap items-start gap-2" data-testid="media-previews">
      {shown.map((p) => (
        <Preview key={p} path={p} />
      ))}
      {!all && media.length > MAX_PREVIEWS && (
        <button className="self-center text-xs text-primary hover:underline" onClick={() => setAll(true)} data-testid="media-more">
          show {media.length - MAX_PREVIEWS} more
        </button>
      )}
    </div>
  );
}

/** One preview: drawn once it scrolls near the screen, after a HEAD request says it exists and isn't too big. */
function Preview({ path }: { path: string }) {
  const { id: pid, root } = useProject();
  const wt = useWorkspace((s) => s.projects[pid]?.wt) ?? root;
  const show = useViewer((s) => s.show);
  const rel = relativeTo(wt, path) ?? relativeTo(root, path);
  const viewWt = rel !== null ? wt : null;
  const url = rawUrl(pid, rel ?? path, viewWt);
  const kind = mediaKind(path)!;
  const name = path.split(/[\\/]/).pop() ?? path;
  const box = useRef<HTMLDivElement>(null);
  const [near, setNear] = useState(false);
  const [state, setState] = useState<"checking" | "ok" | "missing" | "big">("checking");

  useEffect(() => {
    const el = box.current;
    if (!el || near) return;
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        setNear(true);
        io.disconnect();
      }
    }, { rootMargin: "300px" });
    io.observe(el);
    return () => io.disconnect();
  }, [near]);

  useEffect(() => {
    if (!near) return;
    let cancelled = false;
    fetch(url, { method: "HEAD", credentials: "same-origin" })
      .then((r) => {
        if (cancelled) return;
        if (!r.ok) return setState("missing");
        const size = Number(r.headers.get("Content-Length") ?? "0");
        setState(size > MAX_INLINE_BYTES[kind] ? "big" : "ok");
      })
      .catch(() => !cancelled && setState("missing"));
    return () => {
      cancelled = true;
    };
  }, [near, url, kind]);

  const open = () => show(pid, viewWt, rel ?? path);
  if (state === "missing") return null;
  return (
    <div ref={box} className="min-h-10 max-w-full" data-testid="media-preview" data-kind={kind}>
      {state === "big" ? (
        <span className="text-xs text-muted-foreground">{name}: too large to preview here</span>
      ) : !near || state === "checking" ? (
        <div className="flex h-24 w-32 items-center justify-center rounded-md border bg-muted/30 text-muted-foreground">
          <ImageIcon className="size-5 opacity-50" />
        </div>
      ) : kind === "image" ? (
        <button onClick={open} className="block overflow-hidden rounded-md border bg-muted/20" title={`${path} (click to enlarge)`}>
          <img src={url} alt={name} loading="lazy" className="max-h-80 max-w-full object-contain" />
        </button>
      ) : kind === "audio" ? (
        <div className="space-y-1">
          <div className="text-xs text-muted-foreground">{name}</div>
          <audio src={url} controls preload="metadata" className="max-w-full" />
        </div>
      ) : kind === "video" ? (
        <video src={url} controls preload="metadata" className="max-h-80 max-w-full rounded-md border bg-black" title={path} />
      ) : (
        <button
          onClick={open}
          className="flex w-56 items-center gap-3 rounded-md border bg-panel p-3 text-left hover:bg-accent/50"
          title={`${path} (click to open)`}
        >
          <FileText className="size-8 shrink-0 text-destructive/80" />
          <span className="min-w-0">
            <span className="block truncate text-sm font-medium">{name}</span>
            <span className="text-xs text-muted-foreground">PDF</span>
          </span>
        </button>
      )}
    </div>
  );
}
