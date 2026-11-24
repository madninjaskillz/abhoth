import { useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Folder, GitBranch, Github, Link2, Loader2 } from "lucide-react";
import { FolderPicker } from "./FolderPicker";
import { CloneProgress, useClonePolling } from "./CloneProgress";
import { GitHubCloneStep } from "./GitHubClone";
import { api, type CloneStatus, type Project } from "@/lib/api";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { Mascot } from "@/components/Mascot";
import { CheckLine, rememberToken, rememberedToken } from "@/features/git/GitHubToken";
import type { GitHubCheck } from "@/lib/api";

/**
 * The [+] tab's wizard: (a) a folder on the machine openorca runs on (picked by browsing that
 * machine's folders), (b) a repository picked after signing in with GitHub (the server's GitHub
 * App, device flow), or (c) any repository by URL, with an optional pasted token or the machine's
 * own git credentials.
 */
export function AddProjectDialog({ open, onOpenChange, onAdded }: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onAdded: (p: Project | { id: string }) => void;
}) {
  const [mode, setMode] = useState("folder");
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="max-w-xl"
        data-testid="add-project"
        // Escape in an inline box (the new folder's name) closes that box, not the dialog.
        onEscapeKeyDown={(e) => {
          if (e.target instanceof Element && e.target.closest("[data-escape-local]")) e.preventDefault();
        }}
      >
        <div className="flex items-center gap-3">
          <Mascot size={64} glow={false} className="mx-0 shrink-0" />
          <div className="min-w-0">
            <DialogTitle>Add a project</DialogTitle>
            <DialogDescription>Folders are on the machine OpenOrca runs on, not the one this browser is on.</DialogDescription>
          </div>
        </div>
        <Tabs value={mode} onValueChange={setMode} className="flex min-h-0 min-w-0 flex-col gap-3">
          {/* Three tabs share the width: each shrinks and truncates; narrow screens get short labels. */}
          <TabsList className="flex w-full min-w-0">
            <TabsTrigger value="folder" className="min-w-0 flex-1 px-1.5 sm:px-3" title="Open a folder">
              <Folder /><span className="truncate"><span className="sm:hidden">Folder</span><span className="hidden sm:inline">Open a folder</span></span>
            </TabsTrigger>
            <TabsTrigger value="github" className="min-w-0 flex-1 px-1.5 sm:px-3" data-testid="tab-github" title="Clone from GitHub">
              <Github /><span className="truncate"><span className="sm:hidden">GitHub</span><span className="hidden sm:inline">Clone from GitHub</span></span>
            </TabsTrigger>
            <TabsTrigger value="clone" className="min-w-0 flex-1 px-1.5 sm:px-3" title="Clone a repository (URL)">
              <Link2 /><span className="truncate"><span className="sm:hidden">By URL</span><span className="hidden sm:inline">Clone a repository</span></span>
            </TabsTrigger>
          </TabsList>
          <TabsContent value="folder" className="min-h-0 min-w-0">
            {open && <FolderStep onAdded={onAdded} />}
          </TabsContent>
          <TabsContent value="github" className="min-h-0 min-w-0">
            {open && <GitHubCloneStep onAdded={onAdded} />}
          </TabsContent>
          <TabsContent value="clone" className="min-h-0 min-w-0">
            {open && <CloneStep onAdded={onAdded} />}
          </TabsContent>
        </Tabs>
      </DialogContent>
    </Dialog>
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
      <div className="flex min-w-0 items-center justify-end gap-2">
        <span className="mr-auto min-w-0 truncate font-mono text-[11px] text-muted-foreground">{path}</span>
        <Button size="sm" disabled={!path || busy} onClick={() => void add()} data-testid="add-folder">
          {busy && <Loader2 className="animate-spin" />} Open this folder
        </Button>
      </div>
    </div>
  );
}

function CloneStep({ onAdded }: { onAdded: (p: { id: string }) => void }) {
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

  useClonePolling(job, setJob, onAdded, setError);

  const start = async () => {
    setError(null);
    try {
      if (token.trim()) rememberToken(token.trim());
      setJob(await api.clone(url, parent, name || undefined, token.trim() || undefined));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (job) return <CloneProgress job={job} onBack={() => setJob(null)} />;

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
