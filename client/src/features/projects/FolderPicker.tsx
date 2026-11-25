import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowUp, Folder, FolderGit2, FolderOpen, FolderPlus, HardDrive, Loader2 } from "lucide-react";
import { folderNameError } from "./folderName";
import { api } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** Browse folders on the server; Enter or "Open" on a path also works. */
export function FolderPicker({ path, setPath, pickLabel }: { path: string; setPath: (p: string) => void; pickLabel?: string }) {
  const [typed, setTyped] = useState(path);
  useEffect(() => setTyped(path), [path]);
  const listing = useQuery({ queryKey: ["browse", path], queryFn: () => api.browse(path || undefined), retry: false });
  // The desktop app (abhoth --desktop) also offers the system's folder dialog.
  const info = useQuery({ queryKey: ["info"], queryFn: api.info, staleTime: 60_000 });
  const [picking, setPicking] = useState(false);
  const pickNative = async () => {
    setPicking(true);
    try {
      const picked = await api.pickFolder(path || undefined);
      if (picked.path) setPath(picked.path);
    } finally {
      setPicking(false);
    }
  };
  useEffect(() => {
    if (!path && listing.data) setPath(listing.data.path);
  }, [path, listing.data, setPath]);
  const data = listing.data;

  // "New folder": an inline name box; the folder is made inside the one being browsed, then opened.
  const qc = useQueryClient();
  const [naming, setNaming] = useState(false);
  const [newName, setNewName] = useState("");
  const [makeError, setMakeError] = useState<string | null>(null);
  const [making, setMaking] = useState(false);
  const closeNaming = () => {
    setNaming(false);
    setNewName("");
    setMakeError(null);
  };
  useEffect(closeNaming, [path]);
  const makeFolder = async () => {
    const name = newName;
    const problem = folderNameError(name);
    if (problem) return setMakeError(problem);
    if (!data) return;
    setMaking(true);
    setMakeError(null);
    try {
      const made = await api.makeFolder(data.path, name);
      await qc.invalidateQueries({ queryKey: ["browse", path] });
      closeNaming();
      setPath(made.path);
    } catch (e) {
      setMakeError(e instanceof Error ? e.message : String(e));
    } finally {
      setMaking(false);
    }
  };

  return (
    <div className="flex min-h-0 flex-col gap-2">
      <div className="flex gap-1.5">
        <input
          className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && setPath(typed.trim())}
          placeholder="C:\path\to\folder"
          aria-label={pickLabel ?? "Folder path"}
          data-testid="folder-path"
        />
        <Button size="sm" variant="outline" onClick={() => setPath(typed.trim())}>Go</Button>
        {info.data?.desktop && (
          <Button size="sm" variant="ghost" disabled={picking} onClick={() => void pickNative()} title="Choose a folder with the system dialog" data-testid="pick-folder-native">
            {picking ? <Loader2 className="animate-spin" /> : <FolderOpen />}
          </Button>
        )}
        <Button size="sm" variant="ghost" disabled={!data?.parent} onClick={() => data?.parent && setPath(data.parent)} title="Up a folder">
          <ArrowUp />
        </Button>
        <Button
          size="sm"
          variant="ghost"
          disabled={!data}
          onClick={() => (naming ? closeNaming() : setNaming(true))}
          title="New folder here"
          data-testid="new-folder"
        >
          <FolderPlus />
        </Button>
      </div>
      {naming && data && (
        <div className="flex flex-col gap-1">
          <div className="flex gap-1.5">
            <input
              autoFocus
              className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 text-xs outline-none focus:ring-1 focus:ring-ring"
              value={newName}
              onChange={(e) => {
                setNewName(e.target.value);
                setMakeError(null);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  void makeFolder();
                } else if (e.key === "Escape") {
                  e.preventDefault();
                  closeNaming();
                }
              }}
              placeholder={`New folder in ${data.path}`}
              aria-label="New folder name"
              data-testid="new-folder-name"
              data-escape-local
            />
            <Button size="sm" disabled={making || !newName.trim()} onClick={() => void makeFolder()} data-testid="new-folder-create">
              {making && <Loader2 className="animate-spin" />} Create
            </Button>
            <Button size="sm" variant="ghost" onClick={closeNaming}>Cancel</Button>
          </div>
          {makeError && <div className="text-xs text-destructive" data-testid="new-folder-error">{makeError}</div>}
        </div>
      )}
      {data && (
        <div className="flex flex-wrap gap-1">
          {data.places.map((p) => (
            <button key={p.path} onClick={() => setPath(p.path)} className="flex items-center gap-1 rounded border px-1.5 py-0.5 text-[11px] text-muted-foreground hover:bg-accent">
              <HardDrive className="size-3" /> {p.label}
            </button>
          ))}
        </div>
      )}
      <div className="h-56 overflow-y-auto rounded-md border bg-background text-xs" data-testid="folder-list">
        {listing.isLoading && <div className="p-3 text-muted-foreground"><Loader2 className="size-3 animate-spin" /></div>}
        {listing.error && <div className="p-3 text-destructive">{String((listing.error as Error).message)}</div>}
        {data?.folders.length === 0 && <div className="p-3 text-muted-foreground">No sub-folders.</div>}
        {data?.folders.map((f) => (
          <button
            key={f.path}
            onClick={() => setPath(f.path)}
            className="flex h-7 w-full items-center gap-2 px-2 text-left hover:bg-accent/60"
            data-testid="folder-entry"
          >
            {f.isRepo ? <FolderGit2 className="size-3.5 text-primary" /> : <Folder className="size-3.5 text-muted-foreground" />}
            <span className={cn("truncate", f.name.startsWith(".") && "text-muted-foreground")}>{f.name}</span>
            {f.isRepo && <span className="ml-auto text-[10px] text-muted-foreground">git</span>}
            {!f.isRepo && f.repoError && (
              <span className="ml-auto truncate text-[10px] text-warning" title={f.repoError}>git refuses this repository</span>
            )}
          </button>
        ))}
        {data?.truncated && <div className="p-2 text-muted-foreground">… more folders not shown</div>}
      </div>
    </div>
  );
}
