import { useCallback, useEffect, useRef, useState } from "react";
import { brandName } from "@/lib/brand";
import { keepPreviousData, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowLeft, Check, ChevronLeft, ChevronRight, Copy, ExternalLink, GitBranch, Github, Globe, KeyRound, Loader2, Lock, LogOut, RefreshCw, Search,
  TriangleAlert,
} from "lucide-react";
import {
  api, ApiError, type CloneStatus, type GitHubDevicePoll, type GitHubDeviceStart, type GitHubRepo, type GitHubRepoPage, type GitHubUserStatus,
} from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { isAdmin } from "@/lib/auth";
import { FolderPicker } from "./FolderPicker";
import { CloneProgress, useClonePolling } from "./CloneProgress";
import { deviceEndMessage, ownerRepo, pageCount, pageRange, pollDelayMs, repoFolderName, timeAgo } from "./cloneHelpers";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** The server answered "not signed in to GitHub" (409 with signedOut): show the sign-in again. */
const signedOut = (e: unknown) => e instanceof ApiError && (e as ApiError & { data?: { signedOut?: boolean } }).data?.signedOut === true;

/** What the repository picker and the clone step call: the user's own credentials or the server's GitHub App. */
interface RepoOps {
  key: string;
  app: boolean;
  repos: (query: string, page: number, reload: boolean) => Promise<GitHubRepoPage>;
  branches: (repo: string) => Promise<{ defaultBranch?: string | null; branches: string[]; truncated: boolean }>;
  clone: (body: { repo: string; branch?: string; parent: string; name?: string }) => Promise<CloneStatus>;
}

const appOps: RepoOps = { key: "github-app", app: true, repos: api.githubRepos, branches: api.githubBranches, clone: api.githubClone };
const userOps: RepoOps = { key: "github-user", app: false, repos: api.githubUserRepos, branches: api.githubUserBranches, clone: api.githubUserClone };

type Panel = "none" | "token" | "device";

/**
 * "Clone from GitHub" (D147): with the user's own GitHub credentials, no GitHub App needed. In the
 * desktop app (or a local server) an admin's machine sign-ins are used as they are (the GitHub
 * CLI, then Git Credential Manager); anyone can paste a token (for the session, or saved) or sign
 * in with GitHub (device flow, when the server has an OAuth client ID); with nothing, public
 * repositories clone anonymously. Admins can still use the server's GitHub App when it's set up.
 */
export function GitHubCloneStep({ onAdded }: { onAdded: (p: { id: string }) => void }) {
  const [useApp, setUseApp] = useState(false);
  if (useApp && isAdmin()) {
    return (
      <div className="flex flex-col gap-2 text-xs">
        <button className="self-start text-[11px] text-muted-foreground hover:text-foreground hover:underline" onClick={() => setUseApp(false)}>
          <ArrowLeft className="mr-1 inline size-3" />Use your own GitHub sign-in instead
        </button>
        <GitHubCloneAdmin onAdded={onAdded} />
      </div>
    );
  }
  return <UserGitHubClone onAdded={onAdded} onUseApp={() => setUseApp(true)} />;
}

function UserGitHubClone({ onAdded, onUseApp }: { onAdded: (p: { id: string }) => void; onUseApp: () => void }) {
  const qc = useQueryClient();
  const status = useQuery({ queryKey: ["github-user"], queryFn: api.githubUser });
  const refresh = useCallback(() => {
    void qc.invalidateQueries({ queryKey: ["github-user"] });
    void qc.invalidateQueries({ queryKey: ["github-user-repos"] });
  }, [qc]);
  const [panel, setPanel] = useState<Panel>("none");
  const done = useCallback(() => {
    setPanel("none");
    refresh();
  }, [refresh]);

  if (status.isLoading) {
    return (
      <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
        <Loader2 className="size-3.5 animate-spin" /> Looking for your GitHub sign-in…
      </div>
    );
  }
  if (status.error) return <div className="p-3 text-xs text-destructive">{errorText(status.error)}</div>;
  const s = status.data!;
  return (
    <div className="flex flex-col gap-2 text-xs" data-testid="github-user">
      <UserAccount s={s} panel={panel} setPanel={setPanel} onChanged={refresh} />
      {panel === "token" && <TokenForm s={s} onDone={done} onCancel={() => setPanel("none")} />}
      {panel === "device" && (
        <SignIn start={api.githubUserDeviceStart} poll={api.githubUserDevicePoll} onSignedIn={done} autoStart onCancel={() => setPanel("none")} />
      )}
      {panel === "none" && <RepoWizard ops={userOps} anonymous={!s.signedIn} onAdded={onAdded} onSignedOut={refresh} />}
      {s.appAvailable && panel === "none" && (
        <button className="self-start text-[11px] text-muted-foreground hover:text-foreground hover:underline" onClick={onUseApp} data-testid="github-use-app">
          Use the server's GitHub App sign-in instead
        </button>
      )}
    </div>
  );
}

/** Which sign-in is in use, and the ways to change it. */
function UserAccount({ s, panel, setPanel, onChanged }: {
  s: GitHubUserStatus;
  panel: Panel;
  setPanel: (p: Panel) => void;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const run = async (f: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await f();
      onChanged();
    } finally {
      setBusy(false);
    }
  };
  const own = s.source === "session" || s.source === "saved" || s.source === "oauth";
  return (
    <div className="flex flex-col gap-1 rounded-md border bg-muted/30 px-2 py-1.5" data-testid="github-user-account">
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        {s.avatarUrl ? <img src={s.avatarUrl} alt="" className="size-5 rounded-full" referrerPolicy="no-referrer" /> : <Github className="size-4 shrink-0" />}
        <span className="min-w-0 flex-1 truncate" data-testid="github-user-source" title={s.sourceText}>
          {s.sourceText}
          {s.login && <> as <span className="font-medium" data-testid="github-login">@{s.login}</span></>}
        </span>
        <span className="flex shrink-0 items-center gap-0.5">
          {s.machineAllowed && !own && (
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-1.5"
              disabled={busy}
              onClick={() => void run(api.githubUserRecheck)}
              title="Look for this machine's GitHub sign-ins again (after gh auth login)"
            >
              <RefreshCw className={cn(busy && "animate-spin")} />
            </Button>
          )}
          {panel === "none" && (
            <Button size="sm" variant="ghost" className="h-6 px-1.5" onClick={() => setPanel("token")} data-testid="github-user-paste">
              <KeyRound /> {own ? "Change token" : "Paste a token"}
            </Button>
          )}
          {panel === "none" && s.oauthConfigured && (
            <Button size="sm" variant="ghost" className="h-6 px-1.5" onClick={() => setPanel("device")} data-testid="github-user-signin">
              <Github /> Sign in with GitHub
            </Button>
          )}
          {own && panel === "none" && (
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-1.5"
              disabled={busy}
              onClick={() => void run(api.githubUserForget)}
              title={s.saved ? "Delete the saved token or sign-in from this server" : "Stop using the pasted token"}
              data-testid="github-user-forget"
            >
              <LogOut /> Forget
            </Button>
          )}
        </span>
      </div>
      {s.notes.map((n) => (
        <div key={n} className="flex items-center gap-1 text-[11px] text-amber-600 dark:text-amber-400"><TriangleAlert className="size-3 shrink-0" />{n}</div>
      ))}
      {!s.signedIn && (
        <div className="text-[11px] text-muted-foreground" data-testid="github-user-hint">
          {s.machineAllowed
            ? "No GitHub CLI or Git Credential Manager sign-in found on this machine. "
            : s.machineServer
              ? ""
              : "This server never uses its own machine's GitHub sign-ins for you. "}
          Public repositories clone as they are; for private ones, paste a token{s.oauthConfigured ? " or sign in with GitHub" : ""}.
        </div>
      )}
    </div>
  );
}

/** Paste a token: for this session, or saved (sealed) for this user on the server. */
function TokenForm({ s, onDone, onCancel }: { s: GitHubUserStatus; onDone: () => void; onCancel: () => void }) {
  const [token, setToken] = useState("");
  const [save, setSave] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.githubUserToken(token.trim(), save);
      setToken("");
      onDone();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flex flex-col gap-1.5 rounded-md border p-2" data-testid="github-user-token">
      <span className="text-muted-foreground">
        A GitHub token: fine-grained with Contents read and write on the repositories, or classic with <code>repo</code>.
      </span>
      <div className="flex items-center gap-1.5">
        <input
          type="password"
          autoComplete="off"
          autoFocus
          className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && token.trim()) void submit();
          }}
          placeholder="github_pat_…"
          data-testid="github-user-token-input"
        />
        <Button size="sm" disabled={!token.trim() || busy} onClick={() => void submit()} data-testid="github-user-token-use">
          {busy && <Loader2 className="animate-spin" />} Use token
        </Button>
        <Button size="sm" variant="ghost" onClick={onCancel}>Cancel</Button>
      </div>
      <label className="flex items-start gap-1.5 text-[11px]">
        <input type="checkbox" className="mt-0.5" checked={save} onChange={(e) => setSave(e.target.checked)} data-testid="github-user-token-save" />
        <span>
          Remember it on this server for my account{" "}
          <span className="text-muted-foreground">
            ({s.protection === "dpapi" ? "encrypted with Windows data protection" : "in a file only the server can read"}). Otherwise it's
            kept only until the server restarts.
          </span>
        </span>
      </label>
      {error && <div className="text-destructive" data-testid="github-user-token-error">{error}</div>}
    </div>
  );
}

function GitHubCloneAdmin({ onAdded }: { onAdded: (p: { id: string }) => void }) {
  const qc = useQueryClient();
  const status = useQuery({ queryKey: ["github-app"], queryFn: api.githubApp });
  const refresh = useCallback(() => void qc.invalidateQueries({ queryKey: ["github-app"] }), [qc]);

  if (status.isLoading) return <div className="p-3 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /></div>;
  if (status.error) return <div className="p-3 text-xs text-destructive">{errorText(status.error)}</div>;
  const s = status.data!;
  if (!s.configured) {
    return (
      <div className="flex flex-col gap-2 rounded-md border bg-muted/30 p-3 text-xs" data-testid="github-not-configured">
        <div className="flex items-center gap-2 font-medium"><Github className="size-4" /> GitHub sign-in isn't configured on this server</div>
        <p className="text-muted-foreground">
          It needs a GitHub App with device flow enabled. Register one (GitHub → Settings → Developer settings → GitHub Apps), tick
          "Enable Device Flow", give it Contents read &amp; write, then give {brandName()} its client ID and slug:
        </p>
        <pre className="whitespace-pre-wrap rounded border bg-background p-2 font-mono text-[11px]">{s.setup}</pre>
        <p className="text-muted-foreground">Until then, use "Clone a repository" with a URL (and a token if it's private).</p>
      </div>
    );
  }
  if (!s.signedIn) return <SignIn start={api.githubDeviceStart} poll={api.githubDevicePoll} onSignedIn={refresh} />;
  return (
    <div className="flex flex-col gap-2 text-xs">
      <Account login={s.login} avatarUrl={s.avatarUrl} onSignedOut={refresh} />
      <RepoWizard ops={appOps} installUrl={s.installUrl} onAdded={onAdded} onSignedOut={refresh} />
    </div>
  );
}

/** Device flow: show the code and a link to github.com/login/device, then poll the server until done. */
function SignIn({ start: startFlow, poll: pollFlow, onSignedIn, autoStart, onCancel }: {
  start: () => Promise<GitHubDeviceStart>;
  poll: (flowId: string) => Promise<GitHubDevicePoll>;
  onSignedIn: () => void;
  autoStart?: boolean;
  onCancel?: () => void;
}) {
  const [flow, setFlow] = useState<GitHubDeviceStart | null>(null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const start = async () => {
    setStarting(true);
    setError(null);
    try {
      setFlow(await startFlow());
    } catch (e) {
      setError(errorText(e));
    } finally {
      setStarting(false);
    }
  };

  useEffect(() => {
    if (!flow) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async (interval: number) => {
      timer = setTimeout(async () => {
        if (stopped) return;
        try {
          const p = await pollFlow(flow.flowId);
          if (stopped) return;
          if (p.state === "done") return onSignedIn();
          if (p.state === "pending") return void poll(p.interval);
          setFlow(null);
          setError(deviceEndMessage(p.state, p.error));
        } catch (e) {
          if (stopped) return;
          if (e instanceof ApiError && e.status < 500) {
            setFlow(null);
            setError(errorText(e));
          } else {
            void poll(Math.min(interval * 2, 30)); // the server or network hiccuped: back off and keep trying
          }
        }
      }, pollDelayMs(interval));
    };
    void poll(flow.interval);
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [flow, onSignedIn, pollFlow]);

  const started = useRef(false);
  useEffect(() => {
    if (autoStart && !started.current) {
      started.current = true;
      void start();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoStart]);

  const copy = async () => {
    if (!flow) return;
    try {
      await navigator.clipboard.writeText(flow.userCode);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch { /* no clipboard (http on a LAN address): the code is selectable */ }
  };

  if (flow) {
    return (
      <div className="flex flex-col items-center gap-3 rounded-md border p-4 text-xs" data-testid="github-device">
        <div className="text-muted-foreground">Enter this code on GitHub:</div>
        <div className="flex items-center gap-2">
          <code className="select-all rounded-md border bg-background px-3 py-1.5 font-mono text-2xl tracking-widest" data-testid="github-user-code">
            {flow.userCode}
          </code>
          <Button size="sm" variant="outline" onClick={() => void copy()} title="Copy the code" data-testid="github-copy-code">
            {copied ? <Check /> : <Copy />} {copied ? "Copied" : "Copy"}
          </Button>
        </div>
        <a
          href={flow.verificationUri}
          target="_blank"
          rel="noopener noreferrer"
          className="flex items-center gap-1 font-medium text-primary underline-offset-2 hover:underline"
          data-testid="github-verify-link"
        >
          Open {flow.verificationUri.replace(/^https?:\/\//, "")} <ExternalLink className="size-3" />
        </a>
        <div className="flex items-center gap-2 text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" /> Waiting for you to approve on GitHub…
        </div>
        <Button size="sm" variant="ghost" onClick={() => { setFlow(null); onCancel?.(); }}>Cancel</Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col items-center gap-3 rounded-md border p-5 text-center text-xs">
      <Github className="size-8" />
      <p className="max-w-sm text-muted-foreground">
        {autoStart
          ? "Starting the GitHub sign-in…"
          : "Sign in with GitHub to pick a repository and clone it. No token to paste: the sign-in stays on the server and is used for this project's fetch, pull and push."}
      </p>
      <Button size="sm" onClick={() => void start()} disabled={starting} data-testid="github-signin">
        {starting ? <Loader2 className="animate-spin" /> : <Github />} Sign in with GitHub
      </Button>
      {error && <div className="text-destructive" data-testid="github-signin-error">{error}</div>}
      {onCancel && <Button size="sm" variant="ghost" onClick={onCancel}>Cancel</Button>}
    </div>
  );
}

function Account({ login, avatarUrl, onSignedOut }: { login?: string | null; avatarUrl?: string | null; onSignedOut: () => void }) {
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const signOut = async () => {
    setBusy(true);
    try {
      await api.githubSignOut();
      onSignedOut();
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flex flex-col gap-1 rounded-md border bg-muted/30 px-2 py-1.5" data-testid="github-account">
      <div className="flex items-center gap-2">
        {avatarUrl ? <img src={avatarUrl} alt="" className="size-5 rounded-full" referrerPolicy="no-referrer" /> : <Github className="size-4" />}
        <span className="min-w-0 truncate">Signed in as <span className="font-medium" data-testid="github-login">@{login ?? "?"}</span></span>
        <span className="ml-auto" />
        {!confirming && (
          <Button size="sm" variant="ghost" className="h-6 shrink-0 px-1.5" onClick={() => setConfirming(true)} data-testid="github-signout" title="Sign out of GitHub">
            <LogOut /> <span className="hidden sm:inline">Sign out of GitHub</span><span className="sm:hidden">Sign out</span>
          </Button>
        )}
      </div>
      {confirming && (
        <div className="flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
          <span>
            Deletes the sign-in from this server; projects cloned with it can't fetch or push until you sign in again. To revoke
            the app's access too, use{" "}
            <a href="https://github.com/settings/apps/authorizations" target="_blank" rel="noopener noreferrer" className="text-primary hover:underline">
              github.com/settings/apps/authorizations
            </a>.
          </span>
          <Button size="sm" variant="destructive" className="h-6 px-2" disabled={busy} onClick={() => void signOut()} data-testid="github-signout-confirm">
            {busy && <Loader2 className="animate-spin" />} Sign out
          </Button>
          <Button size="sm" variant="ghost" className="h-6 px-2" onClick={() => setConfirming(false)}>Keep</Button>
        </div>
      )}
    </div>
  );
}

/** Pick a repository (search, pages), then a branch, the parent folder and the folder name; clone. */
function RepoWizard({ ops, installUrl, anonymous, onAdded, onSignedOut }: {
  ops: RepoOps;
  installUrl?: string | null;
  /** No credentials: no list, just a repository by name (public ones clone). */
  anonymous?: boolean;
  onAdded: (p: { id: string }) => void;
  onSignedOut: () => void;
}) {
  const [repo, setRepo] = useState<GitHubRepo | null>(null);
  const [job, setJob] = useState<CloneStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  useClonePolling(job, setJob, onAdded, setError);

  if (job) return <CloneProgress job={job} onBack={() => setJob(null)} />;
  if (!repo) {
    return anonymous
      ? <ByName onPick={setRepo} />
      : <RepoList ops={ops} installUrl={installUrl} onPick={setRepo} onSignedOut={onSignedOut} />;
  }
  return (
    <ClonePlace
      ops={ops}
      repo={repo}
      onBack={() => setRepo(null)}
      onSignedOut={onSignedOut}
      onStarted={setJob}
      error={error}
      setError={setError}
    />
  );
}

/** A repository typed as owner/repo (or its github.com URL). */
function ByName({ onPick }: { onPick: (r: GitHubRepo) => void }) {
  const [typed, setTyped] = useState("");
  const picked = ownerRepo(typed);
  const go = () => {
    if (picked) onPick({ fullName: picked, owner: picked.split("/")[0], name: picked.split("/")[1], private: false });
  };
  return (
    <div className="flex flex-col gap-1.5" data-testid="github-by-name">
      <span className="text-muted-foreground">Repository (owner/repo, or its github.com address)</span>
      <div className="flex gap-1.5">
        <input
          autoFocus
          className="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") go();
          }}
          placeholder="owner/repo"
          data-testid="github-by-name-input"
        />
        <Button size="sm" disabled={!picked} onClick={go} data-testid="github-by-name-next">Next</Button>
      </div>
    </div>
  );
}

function RepoList({ ops, installUrl, onPick, onSignedOut }: { ops: RepoOps; installUrl?: string | null; onPick: (r: GitHubRepo) => void; onSignedOut: () => void }) {
  const [typed, setTyped] = useState("");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const reloadNext = useRef(false);
  useEffect(() => {
    const t = setTimeout(() => {
      setQuery(typed.trim());
      setPage(1);
    }, 250);
    return () => clearTimeout(t);
  }, [typed]);
  const repos = useQuery({
    queryKey: [`${ops.key}-repos`, query, page],
    queryFn: () => {
      const reload = reloadNext.current;
      reloadNext.current = false;
      return ops.repos(query, page, reload);
    },
    placeholderData: keepPreviousData,
    retry: false,
  });
  useEffect(() => {
    if (repos.error && signedOut(repos.error)) onSignedOut();
  }, [repos.error, onSignedOut]);
  const reload = () => {
    reloadNext.current = true;
    void repos.refetch();
  };
  const data = repos.data;
  const pages = data ? pageCount(data.perPage, data.total) : 1;
  const noInstall = ops.app && data && data.installations.length === 0;
  const typedRepo = ops.app ? null : ownerRepo(typed);

  return (
    <div className="flex min-h-0 flex-col gap-2">
      <div className="flex gap-1.5">
        <div className="flex h-8 min-w-0 flex-1 items-center gap-1.5 rounded-md border bg-background px-2 focus-within:ring-1 focus-within:ring-ring">
          <Search className="size-3.5 shrink-0 text-muted-foreground" />
          <input
            autoFocus
            className="min-w-0 flex-1 bg-transparent text-xs outline-none"
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            placeholder={ops.app ? "Search repositories" : "Search your repositories, or type owner/repo"}
            aria-label="Search repositories"
            data-testid="github-repo-search"
          />
        </div>
        <Button size="sm" variant="ghost" onClick={reload} disabled={repos.isFetching} title="Refresh the list from GitHub" data-testid="github-repo-refresh">
          <RefreshCw className={cn(repos.isFetching && "animate-spin")} />
        </Button>
      </div>
      {noInstall ? (
        <div className="flex flex-col items-center gap-2 rounded-md border p-4 text-center" data-testid="github-install">
          <p className="text-muted-foreground">The {brandName()} GitHub App isn't installed on your account or any organization yet, so it can't see any repositories.</p>
          {!ops.app && <span className="min-w-0 truncate">Not listed? Type owner/repo.</span>}
          {ops.app && installUrl && (
            <a href={installUrl} target="_blank" rel="noopener noreferrer" className="flex items-center gap-1 font-medium text-primary hover:underline">
              Install the app on your account or organization <ExternalLink className="size-3" />
            </a>
          )}
          <Button size="sm" variant="outline" onClick={reload}><RefreshCw /> I've installed it: refresh</Button>
        </div>
      ) : (
        <div className="h-64 overflow-y-auto overflow-x-hidden rounded-md border bg-background" data-testid="github-repo-list">
          {repos.isLoading && <div className="p-3 text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /></div>}
          {repos.error && !signedOut(repos.error) && <div className="p-3 text-destructive">{errorText(repos.error)}</div>}
          {typedRepo && !data?.items.some((r) => r.fullName.toLowerCase() === typedRepo.toLowerCase()) && (
            <button
              onClick={() => onPick({ fullName: typedRepo, owner: typedRepo.split("/")[0], name: typedRepo.split("/")[1], private: false })}
              className="flex w-full items-center gap-1.5 border-b px-2 py-1.5 text-left hover:bg-accent/60"
              data-testid="github-repo-by-name"
            >
              <Github className="size-3 shrink-0 text-muted-foreground" /> Clone <span className="font-medium">{typedRepo}</span> by name
            </button>
          )}
          {data?.items.length === 0 && !typedRepo && <div className="p-3 text-muted-foreground">{query ? `No repositories match "${query}".` : "No repositories."}</div>}
          {data?.items.map((r) => (
            <button
              key={r.fullName}
              onClick={() => onPick(r)}
              className="flex w-full min-w-0 flex-col gap-0.5 border-b px-2 py-1.5 text-left last:border-b-0 hover:bg-accent/60"
              title={r.description ? `${r.fullName}: ${r.description}` : r.fullName}
              data-testid="github-repo"
            >
              <div className="flex w-full min-w-0 items-center gap-1.5">
                {r.private ? <Lock className="size-3 shrink-0 text-muted-foreground" /> : <Globe className="size-3 shrink-0 text-muted-foreground" />}
                <span className="min-w-0 truncate">
                  <span className="text-muted-foreground">{r.owner}/</span>
                  <span className="font-medium">{r.name}</span>
                </span>
                <span className="ml-auto shrink-0 rounded border px-1 text-[10px] text-muted-foreground">{r.private ? "private" : "public"}</span>
              </div>
              <div className="flex w-full min-w-0 items-center gap-2 whitespace-nowrap pl-[18px] text-[11px] text-muted-foreground">
                {r.defaultBranch && (
                  <span className="flex min-w-0 max-w-[40%] items-center gap-0.5"><GitBranch className="size-3 shrink-0" /><span className="truncate">{r.defaultBranch}</span></span>
                )}
                {r.updatedAt && <span className="shrink-0">updated {timeAgo(r.updatedAt)}</span>}
                {/* Beside the meta line when there's room; on its own line under it on narrow screens. */}
                {r.description && <span className="hidden min-w-0 flex-1 truncate sm:block" data-testid="github-repo-desc">· {r.description}</span>}
              </div>
              {r.description && <div className="w-full min-w-0 truncate pl-[18px] text-[11px] text-muted-foreground sm:hidden">{r.description}</div>}
            </button>
          ))}
        </div>
      )}
      {!noInstall && (
        <div className="flex min-w-0 items-center gap-2 whitespace-nowrap text-[11px] text-muted-foreground">
          <span className="shrink-0" data-testid="github-repo-range">{data ? pageRange(data.page, data.perPage, data.total) : ""}</span>
          {installUrl && (
            <a href={installUrl} target="_blank" rel="noopener noreferrer" className="min-w-0 truncate hover:text-foreground hover:underline" title="Install the app on another account or organization, or choose which repositories it can see">
              Missing a repository? Configure the app
            </a>
          )}
          <span className="ml-auto" />
          <Button size="sm" variant="ghost" className="h-6 px-1" disabled={page <= 1} onClick={() => setPage((p) => p - 1)} title="Previous page">
            <ChevronLeft />
          </Button>
          <span>{page} / {pages}</span>
          <Button size="sm" variant="ghost" className="h-6 px-1" disabled={page >= pages} onClick={() => setPage((p) => p + 1)} title="Next page" data-testid="github-next-page">
            <ChevronRight />
          </Button>
        </div>
      )}
    </div>
  );
}

function ClonePlace({ ops, repo, onBack, onSignedOut, onStarted, error, setError }: {
  ops: RepoOps;
  repo: GitHubRepo;
  onBack: () => void;
  onSignedOut: () => void;
  onStarted: (j: CloneStatus) => void;
  error: string | null;
  setError: (e: string | null) => void;
}) {
  const branches = useQuery({ queryKey: [`${ops.key}-branches`, repo.fullName], queryFn: () => ops.branches(repo.fullName), retry: false });
  const [branch, setBranch] = useState("");
  const [parent, setParent] = useState("");
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const fallback = repo.defaultBranch ?? "";
  const options = branches.data?.branches ?? (fallback ? [fallback] : []);
  const chosen = branch || branches.data?.defaultBranch || fallback;
  const suggested = repoFolderName(repo.fullName);
  useEffect(() => {
    if (branches.error && signedOut(branches.error)) onSignedOut();
  }, [branches.error, onSignedOut]);

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      onStarted(await ops.clone({ repo: repo.fullName, branch: chosen || undefined, parent, name: name.trim() || undefined }));
    } catch (e) {
      if (signedOut(e)) onSignedOut();
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-2">
      <div className="flex min-w-0 items-center gap-2">
        <Button size="sm" variant="ghost" className="h-6 shrink-0 px-1.5" onClick={onBack} data-testid="github-repo-back"><ArrowLeft /> Repositories</Button>
        {repo.private ? <Lock className="size-3 shrink-0 text-muted-foreground" /> : <Globe className="size-3 shrink-0 text-muted-foreground" />}
        <span className="min-w-0 truncate font-medium" data-testid="github-picked">{repo.fullName}</span>
      </div>
      <label className="flex items-center gap-2">
        <span className="shrink-0 text-muted-foreground"><GitBranch className="mr-1 inline size-3" />Branch</span>
        <select
          className="h-7 min-w-0 flex-1 rounded-md border bg-background px-1.5 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={chosen}
          onChange={(e) => setBranch(e.target.value)}
          disabled={branches.isLoading}
          data-testid="github-branch"
        >
          {options.map((b) => (
            <option key={b} value={b}>{b}{b === (branches.data?.defaultBranch ?? fallback) ? " (default)" : ""}</option>
          ))}
        </select>
        {branches.isLoading && <Loader2 className="size-3.5 animate-spin text-muted-foreground" />}
      </label>
      {branches.data?.truncated && <div className="text-[11px] text-muted-foreground">Only the first 500 branches are listed.</div>}
      {branches.error && !signedOut(branches.error) && <div className="text-[11px] text-destructive">{errorText(branches.error)}</div>}
      <span className="text-muted-foreground">Clone into (on the machine {brandName()} runs on)</span>
      <FolderPicker path={parent} setPath={setParent} pickLabel="Parent folder" />
      <label className="flex items-center gap-2">
        <span className="shrink-0 text-muted-foreground">Folder name</span>
        <input
          className="h-7 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={suggested}
          data-testid="github-clone-name"
        />
      </label>
      {error && <div className="text-destructive" data-testid="github-clone-error">{error}</div>}
      <div className="flex min-w-0 items-center justify-end gap-2">
        <span className="mr-auto min-w-0 truncate font-mono text-[11px] text-muted-foreground">
          {parent ? `${parent.replace(/[\\/]+$/, "")}${parent.includes("\\") ? "\\" : "/"}${name.trim() || suggested}` : ""}
        </span>
        <Button size="sm" disabled={!parent || busy} onClick={() => void start()} data-testid="github-clone-start">
          {busy ? <Loader2 className="animate-spin" /> : <Github />} Clone
        </Button>
      </div>
    </div>
  );
}
