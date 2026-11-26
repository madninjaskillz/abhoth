// Folder browsing on a server (D152 follow-up): a text box with a Browse button, and the browser
// dialog (breadcrumbs, up, refresh, folders first, model files highlighted with sizes, free space,
// new folder, type a path, use this folder). The router lists the server's own filesystem through
// its executor. A server that does not offer it keeps the plain text box.
import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowUp, Check, ChevronRight, File, Folder, FolderOpen, FolderPlus, HardDrive, Link2, Loader2, RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import { RouterApiError, routerApi } from "../routerApi";
import { input } from "./formKit";
import { cleanPath, crumbs, fmtBytes, joinPath, statLine, validFolderName, type DirEntry, type DirListing, type PathStat } from "./inventoryModel";

export const NOT_ENABLED = "Folder browsing isn't enabled on this server.";
const notEnabled = (e: unknown) => e instanceof RouterApiError && e.status === 501;

/** A path box with Browse (when the server offers it) and a check of the typed path. */
export function PathField({ server, value, onChange, mode = "folder", placeholder, testid }: {
  server?: string; value: string; onChange: (v: string) => void; mode?: "folder" | "file"; placeholder?: string; testid?: string;
}) {
  const [open, setOpen] = useState(false);
  const [disabled, setDisabled] = useState(false);
  const [check, setCheck] = useState<{ ok: boolean; text: string } | null>(null);
  const local = cleanPath(value);
  const verify = async () => {
    if (!server || !value.trim()) return setCheck(null);
    if (!local.ok) return setCheck({ ok: false, text: local.error });
    try {
      setCheck(statLine(await routerApi.pathStat(server, local.path), mode));
    } catch (e) {
      if (notEnabled(e)) { setDisabled(true); setCheck(null); } else setCheck({ ok: false, text: e instanceof Error ? e.message : String(e) });
    }
  };
  return (
    <div className="space-y-1">
      <div className="flex gap-1">
        <input className={cn(input, "min-w-0 flex-1 font-mono")} value={value} placeholder={placeholder} spellCheck={false}
          onChange={(e) => { onChange(e.target.value); setCheck(null); }} onBlur={() => void verify()} data-testid={testid} />
        {server && !disabled && (
          <Button type="button" size="sm" variant="outline" className="h-8 shrink-0" onClick={() => setOpen(true)} data-testid={testid ? `${testid}-browse` : "path-browse"}>
            <FolderOpen /> Browse
          </Button>
        )}
      </div>
      {check && <span className={cn("block text-[11px]", check.ok ? "text-muted-foreground" : "text-destructive")} data-testid="path-check">{check.text}</span>}
      {disabled && <span className="block text-[11px] text-muted-foreground" data-testid="browse-disabled">{NOT_ENABLED} Type the path instead.</span>}
      {!server && <span className="block text-[11px] text-muted-foreground">Save the server first to browse its folders.</span>}
      {open && server && (
        <FolderBrowser server={server} start={!local.ok ? "/" : mode === "file" ? parentOf(local.path) : local.path} selected={mode === "file" && local.ok ? local.path : null} mode={mode}
          onClose={() => setOpen(false)}
          onDisabled={() => { setDisabled(true); setOpen(false); }}
          onPick={(p) => { onChange(p); setOpen(false); setCheck(null); }} />
      )}
    </div>
  );
}

/** The folder a file is in. */
export function parentOf(p: string): string {
  return crumbs(p).slice(-2, -1)[0]?.path ?? "/";
}

export function FolderBrowser({ server, start, selected = null, mode, onClose, onPick, onDisabled }: {
  server: string; start: string; selected?: string | null; mode: "folder" | "file"; onClose: () => void; onPick: (p: string) => void; onDisabled: () => void;
}) {
  const qc = useQueryClient();
  const [path, setPath] = useState(start);
  const [typed, setTyped] = useState(start);
  const [pages, setPages] = useState(1);
  const [picked, setPicked] = useState<string | null>(selected);
  const [newName, setNewName] = useState<string | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const key = ["router-browse", server, path, pages];
  const q = useQuery({
    queryKey: key,
    retry: false,
    queryFn: async (): Promise<DirListing> => {
      const first = await routerApi.browse(server, path, 0);
      let all = first;
      for (let i = 1; i < pages && all.more; i++) {
        const next = await routerApi.browse(server, path, all.entries.length);
        all = { ...next, entries: [...all.entries, ...next.entries] };
      }
      return all;
    },
  });
  useEffect(() => {
    if (q.error && notEnabled(q.error)) onDisabled();
  }, [q.error, onDisabled]);
  // a start folder that does not exist (yet): go up to the nearest one that does
  useEffect(() => {
    if (q.error && !notEnabled(q.error) && path !== "/" && path === start) {
      const up = parentOf(path);
      setPath(up);
      setTyped(up);
    }
  }, [q.error, path, start]);
  const go = (p: string) => { setPath(p); setTyped(p); setPages(1); setPicked(null); setMsg(null); };
  const d = q.data;
  const goTyped = async () => {
    const c = cleanPath(typed);
    if (!c.ok) return setMsg(c.error);
    try {
      const s: PathStat = await routerApi.pathStat(server, c.path);
      if (s.exists && s.dir) return go(c.path);
      if (s.exists && mode === "file") return setPicked(c.path), setMsg(null);
      setMsg(statLine(s, mode).text);
    } catch (e) { setMsg(e instanceof Error ? e.message : String(e)); }
  };
  const make = async () => {
    const err = validFolderName(newName ?? "");
    if (err) return setMsg(err);
    try {
      await routerApi.mkdir(server, joinPath(path, newName!));
      setNewName(null);
      setMsg(null);
      await qc.invalidateQueries({ queryKey: ["router-browse", server, path] });
      go(joinPath(path, newName!));
    } catch (e) { setMsg(e instanceof Error ? e.message : String(e)); }
  };
  const click = (e: DirEntry) => {
    if (e.outside) return setMsg(`${e.name} leads outside the folders this server allows`);
    if (e.dir) return go(e.path);
    if (mode === "file") setPicked(e.path);
  };
  const use = mode === "folder" ? path : picked;
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="flex max-h-[90vh] max-w-2xl flex-col gap-2 overflow-hidden p-4 sm:p-6" data-testid="folder-browser">
        <DialogTitle>{mode === "folder" ? "Choose a folder" : "Choose a file"} on {server}</DialogTitle>
        <DialogDescription className="text-xs">The server's own folders, as its engine user sees them.</DialogDescription>
        <div className="flex flex-wrap items-center gap-0.5 text-xs" data-testid="browse-crumbs">
          {crumbs(path).map((c, i, all) => (
            <span key={c.path} className="flex items-center">
              <button className={cn("rounded px-1 py-0.5 font-mono hover:bg-accent", i === all.length - 1 && "font-semibold")} onClick={() => go(c.path)}>{c.label}</button>
              {i < all.length - 1 && i > 0 && <ChevronRight className="size-3 text-muted-foreground" />}
            </span>
          ))}
        </div>
        <div className="flex flex-wrap items-center gap-1">
          <Button size="sm" variant="ghost" className="h-7 px-2" disabled={!d?.parent} onClick={() => d?.parent && go(d.parent)} aria-label="Up" data-testid="browse-up"><ArrowUp /></Button>
          <Button size="sm" variant="ghost" className="h-7 px-2" onClick={() => void q.refetch()} aria-label="Refresh" data-testid="browse-refresh"><RefreshCw className={cn(q.isFetching && "animate-spin")} /></Button>
          <form className="flex min-w-0 flex-1 gap-1" onSubmit={(e) => { e.preventDefault(); void goTyped(); }}>
            <input className={cn(input, "h-7 min-w-0 flex-1 font-mono text-xs")} value={typed} onChange={(e) => setTyped(e.target.value)} spellCheck={false} data-testid="browse-path" />
            <Button size="sm" variant="outline" className="h-7" type="submit">Go</Button>
          </form>
        </div>
        <div className="min-h-[12rem] flex-1 overflow-y-auto rounded-md border" data-testid="browse-list">
          {q.isLoading && <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /> Reading the folder…</div>}
          {q.isError && !notEnabled(q.error) && <div className="p-3 text-xs text-destructive">{q.error instanceof Error ? q.error.message : String(q.error)}</div>}
          {d && d.entries.length === 0 && <div className="p-3 text-xs text-muted-foreground">An empty folder.</div>}
          {d?.entries.map((e) => (
            <button key={e.path} onClick={() => click(e)} onDoubleClick={() => mode === "file" && !e.dir && !e.outside && onPick(e.path)}
              className={cn("flex w-full items-center gap-2 border-b px-2 py-1.5 text-left text-xs last:border-b-0 hover:bg-accent/40",
                !e.dir && !e.model && "text-muted-foreground", e.outside && "opacity-50", picked === e.path && "bg-primary/10")}
              data-testid="browse-entry" data-dir={e.dir} data-model={e.model}>
              {e.dir ? <Folder className="size-3.5 shrink-0 text-primary" /> : e.model ? <HardDrive className="size-3.5 shrink-0 text-success" /> : <File className="size-3.5 shrink-0" />}
              <span className={cn("min-w-0 flex-1 truncate", e.model && "font-medium text-foreground")}>{e.name}</span>
              {e.link && <Link2 className="size-3 shrink-0" aria-label="link" />}
              {!e.dir && e.size != null && <span className="shrink-0 tabular-nums">{fmtBytes(e.size)}</span>}
            </button>
          ))}
          {d?.more && <button className="w-full px-2 py-1.5 text-xs text-primary hover:bg-accent/40" onClick={() => setPages(pages + 1)} data-testid="browse-more">Show more ({d.total - d.entries.length} left)</button>}
          {d?.truncated && <div className="px-2 py-1.5 text-[11px] text-warning">This folder has more entries than can be listed; type a path to go deeper.</div>}
        </div>
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground" data-testid="browse-status">
          {d?.free != null && <span>{fmtBytes(d.free)} free on this volume</span>}
          {d && <span>{d.writable ? "writable" : "read only"} for the engine user</span>}
          {d && <span>{d.total} entr{d.total === 1 ? "y" : "ies"}</span>}
        </div>
        {newName !== null && (
          <form className="flex gap-1" onSubmit={(e) => { e.preventDefault(); void make(); }}>
            <input className={cn(input, "h-7 min-w-0 flex-1 text-xs")} value={newName} onChange={(e) => setNewName(e.target.value)} placeholder="new folder name" autoFocus data-testid="browse-new-name" />
            <Button size="sm" className="h-7" type="submit" data-testid="browse-new-create">Create</Button>
            <Button size="sm" variant="ghost" className="h-7" type="button" onClick={() => setNewName(null)}>Cancel</Button>
          </form>
        )}
        {msg && <div className="text-xs text-destructive" data-testid="browse-msg">{msg}</div>}
        <div className="flex flex-wrap items-center justify-between gap-2 pt-1">
          <Button size="sm" variant="outline" onClick={() => setNewName("")} disabled={!d?.writable || newName !== null} data-testid="browse-new"><FolderPlus /> New folder</Button>
          <div className="flex gap-2">
            <Button size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
            <Button size="sm" disabled={!use || !d} onClick={() => use && onPick(use)} data-testid="browse-use">
              <Check /> {mode === "folder" ? "Use this folder" : "Use this file"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
