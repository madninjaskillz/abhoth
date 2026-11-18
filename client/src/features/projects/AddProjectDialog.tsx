import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowUp, CheckCircle2, Folder, FolderGit2, GitBranch, Github, HardDrive, Loader2, XCircle } from "lucide-react";
import { api, type CloneStatus, type Project } from "@/lib/api";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { cn } from "@/lib/utils";
import { Mascot } from "@/components/Mascot";
import { CheckLine, rememberToken, rememberedToken } from "@/features/git/GitHubToken";
import type { GitHubCheck } from "@/lib/api";

/**
 * The [+] tab's wizard: (a) a folder on the machine openorca runs on (picked by browsing that
 * machine's folders), or (b) a repository cloned there with the machine's own git credentials.
 */
export function AddProjectDialog({ open, onOpenChange, onAdded }: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onAdded: (p: Project | { id: string }) => void;
}) {
  const [mode, setMode] = useState("folder");
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl" data-testid="add-project">
        <div className="flex items-center gap-3">
          <Mascot size={64} glow={false} className="mx-0 shrink-0" />
          <div className="min-w-0">
            <DialogTitle>Add a project</DialogTitle>
            <DialogDescription>Folders are on the machine OpenOrca runs on, not the one this browser is on.</DialogDescription>
          </div>
        </div>
        <Tabs value={mode} onValueChange={setMode} className="flex min-h-0 flex-col gap-3">
          <TabsList className="w-full">
            <TabsTrigger value="folder" className="flex-1"><Folder /> Open a folder</TabsTrigger>
            <TabsTrigger value="clone" className="flex-1"><Github /> Clone a repository</TabsTrigger>
          </TabsList>
          <TabsContent value="folder" className="min-h-0">
            {open && <FolderStep onAdded={onAdded} />}
          </TabsContent>
          <TabsContent value="clone" className="min-h-0">
            {open && <CloneStep onAdded={onAdded} />}
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}

/** Browse folders on the server; Enter or "Open" on a path also works. */
function FolderPicker({ path, setPath, pickLabel }: { path: string; setPath: (p: string) => void; pickLabel?: string }) {
  const [typed, setTyped] = useState(path);
  useEffect(() => setTyped(path), [path]);
  const listing = useQuery({ queryKey: ["browse", path], queryFn: () => api.browse(path || undefined), retry: false });
  useEffect(() => {
    if (!path && listing.data) setPath(listing.data.path);
  }, [path, listing.data, setPath]);
  const data = listing.data;
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
        <Button size="sm" variant="ghost" disabled={!data?.parent} onClick={() => data?.parent && setPath(data.parent)} title="Up a folder">
          <ArrowUp />
        </Button>
      </div>
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

function FolderStep({ onAdded }: { onAdded: (p: Project) => void }) {
  const qc = useQueryClient();
  const [path, setPath] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const p = await api.addFolder(path);
      await qc.invalidateQueries({ queryKey: ["projects"] });
      onAdded(p);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flex flex-col gap-2">
      <FolderPicker path={path} setPath={setPath} />
      {error && <div className="text-xs text-destructive">{error}</div>}
      <div className="flex items-center justify-end gap-2">
        <span className="mr-auto truncate font-mono text-[11px] text-muted-foreground">{path}</span>
        <Button size="sm" disabled={!path || busy} onClick={() => void add()} data-testid="add-folder">
          {busy && <Loader2 className="animate-spin" />} Open this folder
        </Button>
      </div>
    </div>
  );
}

function CloneStep({ onAdded }: { onAdded: (p: { id: string }) => void }) {
  const qc = useQueryClient();
  const [url, setUrl] = useState("");
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [job, setJob] = useState<CloneStatus | null>(null);
  const [token, setToken] = useState(rememberedToken);
  const [check, setCheck] = useState<GitHubCheck | null>(null);
  const [checking, setChecking] = useState(false);

  const validate = async () => {
    setChecking(true);
    try {
      setCheck(await api.validateGithub({ token, url }));
    } catch (e) {
      setCheck({ ok: false, error: e instanceof Error ? e.message : String(e) });
    } finally {
      setChecking(false);
    }
  };

  const suggested = url.trim() ? (url.trim().replace(/\/+$/, "").replace(/\.git$/i, "").split(/[/:\\]/).pop() ?? "") : "";

  useEffect(() => {
    if (!job || job.state !== "running") return;
    const t = setInterval(async () => {
      try {
        const s = await api.cloneStatus(job.id);
        setJob(s);
        if (s.state === "done" && s.projectId) {
          await qc.invalidateQueries({ queryKey: ["projects"] });
          onAdded({ id: s.projectId });
        }
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    }, 400);
    return () => clearInterval(t);
  }, [job, qc, onAdded]);

  const start = async () => {
    setError(null);
    try {
      if (token.trim()) rememberToken(token.trim());
      setJob(await api.clone(url, parent, name || undefined, token.trim() || undefined));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (job) {
    return (
      <div className="flex flex-col gap-2 text-xs" data-testid="clone-progress">
        <div className="flex items-center gap-2">
          {job.state === "running" && <Loader2 className="size-4 animate-spin text-primary" />}
          {job.state === "done" && <CheckCircle2 className="size-4 text-success" />}
          {job.state === "failed" && <XCircle className="size-4 text-destructive" />}
          <span className="truncate font-mono">{job.url}</span>
        </div>
        <div className="truncate text-muted-foreground">into {job.destination}</div>
        <pre className="h-40 overflow-auto whitespace-pre-wrap rounded-md border bg-background p-2 font-mono text-[11px] text-muted-foreground">
          {job.log.join("\n") || "Starting git…"}
        </pre>
        {job.state === "failed" && <div className="text-destructive" data-testid="clone-error">{job.error}</div>}
        <div className="flex justify-end gap-2">
          {job.state === "running" && <Button size="sm" variant="outline" onClick={() => void api.cancelClone(job.id)}>Cancel</Button>}
          {job.state === "failed" && <Button size="sm" variant="outline" onClick={() => setJob(null)}>Back</Button>}
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-2 text-xs">
      <label className="flex flex-col gap-1">
        <span className="text-muted-foreground">Repository (a URL, or owner/repo for GitHub)</span>
        <input
          className="h-8 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="https://github.com/owner/repo.git"
          data-testid="clone-url"
        />
      </label>
      <span className="text-muted-foreground">Clone into (on the machine OpenOrca runs on)</span>
      <FolderPicker path={parent} setPath={setParent} pickLabel="Parent folder" />
      <label className="flex items-center gap-2">
        <span className="shrink-0 text-muted-foreground">Folder name</span>
        <input
          className="h-7 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={suggested || "repository"}
          data-testid="clone-name"
        />
      </label>
      <label className="flex flex-col gap-1">
        <span className="text-muted-foreground">GitHub token (optional: a fine-grained token with access to this repository)</span>
        <div className="flex items-center gap-1.5">
          <input
            type="password"
            autoComplete="off"
            className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
            value={token}
            onChange={(e) => {
              setToken(e.target.value);
              setCheck(null);
            }}
            placeholder="github_pat_…"
            data-testid="clone-token"
          />
          <Button size="sm" variant="outline" disabled={!token.trim() || checking} onClick={() => void validate()} data-testid="clone-token-validate">
            {checking && <Loader2 className="animate-spin" />} Validate
          </Button>
          {rememberedToken() && (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                rememberToken("");
                setToken("");
                setCheck(null);
              }}
              data-testid="clone-token-forget"
            >
              Forget token
            </Button>
          )}
        </div>
      </label>
      {check && <CheckLine check={check} />}
      <p className="text-[11px] text-muted-foreground">
        <GitBranch className="mr-1 inline size-3" />
        {token.trim()
          ? "The token is remembered in this browser and saved (private) with the project on the server, for its fetch, pull and push. It never goes in the URL or git's config."
          : "Without a token, git uses the credentials already set up on that machine (credential manager, gh, SSH keys)."}
      </p>
      {error && <div className="text-destructive" data-testid="clone-error">{error}</div>}
      <div className="flex justify-end">
        <Button size="sm" disabled={!url.trim() || !parent} onClick={() => void start()} data-testid="clone-start">Clone</Button>
      </div>
    </div>
  );
}
