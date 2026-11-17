import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, KeyRound, Loader2, XCircle } from "lucide-react";
import { api, projectApi, type GitHubCheck } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

const TOKEN_KEY = "openorca.githubToken";

/** The token remembered in this browser (prefills the clone wizard); "" when none or storage is off. */
export function rememberedToken(): string {
  try { return localStorage.getItem(TOKEN_KEY) ?? ""; } catch { return ""; }
}

export function rememberToken(token: string) {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch { /* private mode */ }
}

export function CheckLine({ check }: { check: GitHubCheck }) {
  return (
    <div className={cn("flex items-start gap-1 text-[11px]", check.ok ? "text-success" : "text-destructive")} data-testid="github-check">
      {check.ok ? <CheckCircle2 className="mt-px size-3.5 shrink-0" /> : <XCircle className="mt-px size-3.5 shrink-0" />}
      <span>
        {check.ok ? `Signed in to GitHub as ${check.login}` : check.error}
        {check.ok && check.repo && (check.canPush ? ` · can push to ${check.repo}` : ` · ${check.error ?? `read-only on ${check.repo}`}`)}
      </span>
    </div>
  );
}

/** A project's GitHub token, as the git panel shows it: masked, replace, remove, validate. */
export function ProjectGitHubToken({ pid }: { pid: string }) {
  const qc = useQueryClient();
  const papi = projectApi(pid);
  const saved = useQuery({ queryKey: ["github", pid], queryFn: papi.githubToken });
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [check, setCheck] = useState<GitHubCheck | null>(null);
  const done = () => {
    setEditing(false);
    setValue("");
    setError(null);
    void qc.invalidateQueries({ queryKey: ["github", pid] });
  };
  const save = useMutation({ mutationFn: () => papi.setGithubToken(value), onSuccess: done, onError: (e) => setError((e as Error).message) });
  const remove = useMutation({ mutationFn: () => papi.removeGithubToken(), onSuccess: () => { setCheck(null); done(); } });
  const validate = useMutation({ mutationFn: () => api.validateGithub({ pid }), onSuccess: setCheck, onError: (e) => setError((e as Error).message) });

  const token = saved.data?.token;
  return (
    <div className="space-y-1 border-b px-3 py-1.5 text-xs" data-testid="github-token">
      <div className="flex items-center gap-1.5">
        <KeyRound className="size-3.5 text-muted-foreground" />
        <span className="text-muted-foreground">GitHub token</span>
        <span className="truncate font-mono" data-testid="github-token-masked">{token ?? "none"}</span>
        <span className="ml-auto" />
        {token && !editing && (
          <>
            <Button size="sm" variant="ghost" className="h-6 px-1.5" onClick={() => validate.mutate()} disabled={validate.isPending}>
              {validate.isPending ? <Loader2 className="animate-spin" /> : null} Validate
            </Button>
            <Button size="sm" variant="ghost" className="h-6 px-1.5" onClick={() => remove.mutate()}>Remove</Button>
          </>
        )}
        {!editing && (
          <Button size="sm" variant="ghost" className="h-6 px-1.5" onClick={() => setEditing(true)} data-testid="github-token-edit">
            {token ? "Replace" : "Add"}
          </Button>
        )}
      </div>
      {editing && (
        <div className="flex items-center gap-1.5">
          <input
            type="password"
            autoComplete="off"
            className="h-7 min-w-0 flex-1 rounded-md border bg-background px-2 font-mono text-xs outline-none focus:ring-1 focus:ring-ring"
            placeholder="github_pat_…"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            data-testid="github-token-input"
          />
          <Button size="sm" className="h-7" disabled={!value.trim() || save.isPending} onClick={() => save.mutate()}>Save</Button>
          <Button size="sm" variant="ghost" className="h-7" onClick={() => setEditing(false)}>Cancel</Button>
        </div>
      )}
      {error && <div className="text-[11px] text-destructive">{error}</div>}
      {check && <CheckLine check={check} />}
    </div>
  );
}
