import { create } from "zustand";
import { Download, X } from "lucide-react";
import { rawUrl } from "@/lib/api";
import { useProject } from "@/store/session";
import { useWorkspace } from "@/store/workspace";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";

const MEDIA: Record<string, "image" | "pdf" | "audio" | "video"> = {
  png: "image", jpg: "image", jpeg: "image", gif: "image", webp: "image", bmp: "image", ico: "image", avif: "image",
  pdf: "pdf",
  mp3: "audio", wav: "audio", ogg: "audio", oga: "audio", flac: "audio", m4a: "audio", aac: "audio", opus: "audio",
  mp4: "video", m4v: "video", webm: "video", mov: "video", ogv: "video",
};

export function mediaKind(path: string) {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return MEDIA[ext];
}

/**
 * Whether a token in chat output reads as a file path: no spaces or URL scheme, and either a
 * folder separator or a file extension. ":12" / ":12:3" line suffixes are allowed (and dropped).
 */
export function asPath(token: string): string | null {
  const t = token.trim().replace(/^["'`]|["'`]$/g, "");
  if (!t || t.length > 300 || /\s/.test(t) || /^[a-z][a-z0-9+.-]*:\/\//i.test(t)) return null;
  const m = /^((?:[A-Za-z]:[\\/]|~?[\\/]|\.{1,2}[\\/])?[\w@.+\-[\]()]+(?:[\\/][\w@.+\-[\]() ]*[\w@.+\-[\]()])*)(?::\d+(?::\d+)?)?$/.exec(t);
  if (!m) return null;
  const p = m[1];
  const hasSep = /[\\/]/.test(p);
  const hasExt = /\.[A-Za-z0-9]{1,8}$/.test(p) && !/^\d+(\.\d+)+$/.test(p) && !/^v?\d/.test(p.split(/[\\/]/).pop()!);
  if (!hasSep && !hasExt) return null;
  if (hasSep && !hasExt && !/[\\/]$/.test(p) && p.split(/[\\/]/).length < 2) return null;
  return p;
}

/** The path relative to <paramref name="root"/> when inside it ("/" separators), else null for absolute paths outside. */
function relativeTo(root: string, path: string): string | null {
  const norm = (s: string) => s.replace(/\\/g, "/").replace(/\/+$/, "");
  const p = norm(path);
  const r = norm(root);
  const abs = /^([A-Za-z]:\/|\/)/.test(p);
  if (!abs) return p.replace(/^\.\//, "");
  const ci = /^[A-Za-z]:\//.test(r);
  const pc = ci ? p.toLowerCase() : p;
  const rc = ci ? r.toLowerCase() : r;
  return pc === rc ? "" : pc.startsWith(rc + "/") ? p.slice(r.length + 1) : null;
}

interface Viewer {
  open: { pid: string; wt: string | null; path: string } | null;
  show: (pid: string, wt: string | null, path: string) => void;
  close: () => void;
}

export const useViewer = create<Viewer>((set) => ({
  open: null,
  show: (pid, wt, path) => set({ open: { pid, wt, path } }),
  close: () => set({ open: null }),
}));

/** A path in chat output as a link: media opens in the viewer, anything else in an editor tab. */
export function FileLink({ path, children }: { path: string; children?: React.ReactNode }) {
  const { id: pid, root } = useProject();
  const wt = useWorkspace((s) => s.projects[pid]?.wt) ?? root;
  const openFile = useWorkspace((s) => s.openFile);
  const show = useViewer((s) => s.show);
  const rel = relativeTo(wt, path) ?? relativeTo(root, path);
  const media = mediaKind(path);
  if (rel === null && !media) return <>{children ?? path}</>; // outside the project: plain text
  return (
    <a
      href={rawUrl(pid, rel ?? path, rel !== null ? wt : null)}
      className="cursor-pointer text-primary underline decoration-primary/40 underline-offset-2 hover:decoration-primary"
      onClick={(e) => {
        if (e.ctrlKey || e.metaKey || e.button !== 0) return;
        e.preventDefault();
        if (media) show(pid, rel !== null ? wt : null, rel ?? path);
        else openFile(pid, wt, rel!);
      }}
      data-testid="file-link"
      title={path}
    >
      {children ?? path}
    </a>
  );
}

/** The image / PDF / audio / video viewer the file links open. */
export function MediaViewer() {
  const open = useViewer((s) => s.open);
  const close = useViewer((s) => s.close);
  if (!open) return null;
  const url = rawUrl(open.pid, open.path, open.wt);
  const kind = mediaKind(open.path);
  const name = open.path.split(/[\\/]/).pop();
  return (
    <Dialog open onOpenChange={(o) => !o && close()}>
      <DialogContent className="flex max-h-[92vh] w-[min(1100px,96vw)] max-w-none flex-col gap-2 p-3" data-testid="media-viewer">
        <div className="flex items-center gap-2 pr-6">
          <DialogTitle className="truncate font-mono text-sm">{name}</DialogTitle>
          <DialogDescription className="sr-only">{open.path}</DialogDescription>
          <Button size="sm" variant="ghost" className="ml-auto h-7" asChild>
            <a href={url} download={name}><Download /> Download</a>
          </Button>
          <Button size="icon-sm" variant="ghost" onClick={close} aria-label="Close"><X /></Button>
        </div>
        <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto rounded-md bg-muted/30">
          {kind === "image" && <img src={url} alt={name} className="max-h-[80vh] max-w-full object-contain" />}
          {kind === "pdf" && <iframe src={url} title={name} className="h-[80vh] w-full" />}
          {kind === "audio" && <audio src={url} controls className="w-full" />}
          {kind === "video" && <video src={url} controls className="max-h-[80vh] max-w-full" />}
        </div>
      </DialogContent>
    </Dialog>
  );
}
