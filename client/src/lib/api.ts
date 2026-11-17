// REST calls to the server. Auth is an HttpOnly cookie: the token's (opened with ?token=) or, in
// sign-in mode, the session's, plus its CSRF header on state-changing requests.
import { authState, csrfHeaders, sessionEnded } from "@/lib/auth";

export class ApiError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

async function request<T>(method: string, url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    credentials: "same-origin",
    headers: {
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...(method === "GET" ? {} : csrfHeaders()),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data: unknown = undefined;
  try {
    data = text ? JSON.parse(text) : undefined;
  } catch {
    data = undefined;
  }
  if (!res.ok) {
    const err = (data as { error?: string } | undefined)?.error || text || res.statusText;
    if (res.status === 401) sessionEnded();
    const e = new ApiError(
      res.status === 401
        ? authState().mode === "login" ? "Signed out." : "Not signed in: open the URL OpenOrca printed (with ?token=)."
        : err,
      res.status,
    );
    (e as ApiError & { data?: unknown }).data = data;
    throw e;
  }
  return data as T;
}

const q = (params: Record<string, string | boolean | null | undefined>) => {
  const parts = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== "")
    .map(([k, v]) => `${k}=${encodeURIComponent(String(v))}`);
  return parts.length ? `?${parts.join("&")}` : "";
};

export interface GitFile {
  path: string;
  origPath?: string | null;
  index: string;
  workTree: string;
  staged: boolean;
  unstaged: boolean;
  untracked: boolean;
  conflicted: boolean;
}

export interface GitStatus {
  isRepo: boolean;
  root?: string | null;
  branch?: string | null;
  upstream?: string | null;
  ahead: number;
  behind: number;
  error?: string | null;
  files: GitFile[];
}

export interface GitDiff {
  path: string;
  original: string;
  modified: string;
  binary: boolean;
  tooLarge: boolean;
}

export interface GitOpResult {
  ok: boolean;
  output: string;
  error: string;
}

export interface ServerInfo {
  workingDirectory: string;
  mode: "tui" | "headless";
  version?: string;
  terminal: boolean;
  primary: string;
  canAddProjects: boolean;
  caseInsensitivePaths: boolean;
  bench?: boolean;
}

export interface GitHubCheck {
  ok: boolean;
  login?: string | null;
  repo?: string | null;
  canPush?: boolean | null;
  error?: string | null;
}

export interface BenchArm {
  name: string;
  leadMode: string;
  set?: string[] | null;
}

export interface BenchSuite {
  name: string;
  arms: BenchArm[];
  tasks: string[];
  reps: number;
  timeoutSeconds?: number | null;
  description?: string | null;
}

export interface BenchRunInfo {
  id: string;
  suite: string;
  arm: string;
  task: string;
  rep: number;
  status: string;
  note?: string | null;
  projectId?: string | null;
  startedAt?: string | null;
  endedAt?: string | null;
}

export interface BenchState {
  paused: boolean;
  gate?: string | null;
  current?: BenchRunInfo | null;
  queue: BenchRunInfo[];
  recent: BenchRunInfo[];
  suites: BenchSuite[];
}

export interface BenchResult {
  id: string;
  suite: string;
  arm: string;
  lead_mode: string;
  task: string;
  rep: number;
  status: string;
  pass?: boolean;
  reason?: string;
  note?: string | null;
  tests_passed?: number;
  tests_total?: number;
  started?: string;
  wall_s?: number;
  requests?: number;
  prompt_tokens?: number;
  cached_tokens?: number;
  output_tokens?: number;
  decode_tps?: number | null;
  lead_tps?: number | null;
  helpers?: number;
  mean_w?: number | null;
}

export interface Project {
  id: string;
  name: string;
  path: string;
  primary: boolean;
  mode: "tui" | "headless" | "worker";
  exists: boolean;
  /** A bench run's tab: watched, not driven. */
  readOnly?: boolean;
}

export interface ProjectList {
  primary: string;
  canAdd: boolean;
  projects: Project[];
}

export interface Worktree {
  path: string;
  worktreePath: string;
  branch?: string | null;
  head?: string | null;
  main: boolean;
  detached: boolean;
  locked: boolean;
  prunable: boolean;
}

export interface FileEntry {
  name: string;
  path: string;
  dir: boolean;
  ignored: boolean;
  size?: number | null;
}

export interface FileListing {
  path: string;
  entries: FileEntry[];
  truncated: boolean;
}

export type ChangeKind = "modified" | "added" | "deleted" | "renamed" | "untracked" | "conflict";

export interface ChangeEntry {
  path: string;
  kind: ChangeKind;
  index: string;
  workTree: string;
}

export interface Changes {
  isRepo: boolean;
  branch?: string | null;
  folder: string;
  files: ChangeEntry[];
}

export interface EditMarkers {
  editing: string[];
  edited: string[];
}

export interface FileContent {
  path: string;
  content: string;
  etag: string;
  binary: boolean;
  tooLarge: boolean;
  bom: boolean;
  size: number;
}

export interface FileWriteResult {
  ok: boolean;
  etag?: string | null;
  conflict: boolean;
  error?: string | null;
}

export interface FolderListing {
  path: string;
  parent?: string | null;
  folders: { name: string; path: string; isRepo: boolean }[];
  places: { label: string; path: string }[];
  truncated: boolean;
}

export interface CloneStatus {
  id: string;
  url: string;
  destination: string;
  state: "running" | "done" | "failed";
  progress?: string | null;
  log: string[];
  error?: string | null;
  projectId?: string | null;
}

export const api = {
  info: () => request<ServerInfo>("GET", "/api/info"),
  projects: () => request<ProjectList>("GET", "/api/projects"),
  addFolder: (path: string, name?: string) => request<Project>("POST", "/api/projects", { path, name }),
  removeProject: (id: string) => request<{ ok: boolean }>("DELETE", `/api/projects/${id}`),
  bench: () => request<BenchState>("GET", "/api/bench"),
  benchResults: () => request<BenchResult[]>("GET", "/api/bench/results"),
  benchStart: (suite: string) => request<{ queued: number }>("POST", "/api/bench/start", { suite }),
  benchControl: (what: "pause" | "resume" | "stop") => request<{ ok: boolean }>("POST", `/api/bench/${what}`),
  benchOutput: async (id: string) => {
    const res = await fetch(`/api/bench/runs/${encodeURIComponent(id)}/output`, { credentials: "same-origin" });
    return res.ok ? res.text() : "(no checker output for this run)";
  },
  browse: (path?: string) => request<FolderListing>("GET", `/api/fs/browse${q({ path })}`),
  clone: (url: string, parent: string, name?: string, token?: string) =>
    request<CloneStatus>("POST", "/api/projects/clone", { url, parent, name, token: token || undefined }),
  validateGithub: (body: { token?: string; pid?: string; url?: string }) => request<GitHubCheck>("POST", "/api/github/validate", body),
  benchMirrorState: () =>
    request<{ exists: boolean; path: string; head?: string; origin?: string | null; updated?: string }>("GET", "/api/bench/mirror"),
  benchMirror: () => request<{ ok: boolean; output: string; error: string }>("POST", "/api/bench/mirror"),
  cloneStatus: (id: string) => request<CloneStatus>("GET", `/api/clone/${id}`),
  cancelClone: (id: string) => request<{ ok: boolean }>("DELETE", `/api/clone/${id}`),
};

/** Calls scoped to one project and (optionally) one of its worktrees. */
export function projectApi(pid: string, wt?: string | null) {
  const base = `/api/projects/${pid}`;
  return {
    worktrees: () => request<Worktree[]>("GET", `${base}/worktrees`),
    tree: (dir: string, ignored: boolean) => request<FileListing>("GET", `${base}/tree${q({ wt, dir, ignored: ignored || undefined })}`),
    changes: () => request<Changes>("GET", `${base}/changes${q({ wt })}`),
    markers: () => request<EditMarkers>("GET", `${base}/markers`),
    readFile: (path: string) => request<FileContent>("GET", `${base}/file${q({ wt, path })}`),
    writeFile: (path: string, content: string, etag: string | null, bom: boolean) =>
      request<FileWriteResult>("PUT", `${base}/file${q({ wt })}`, { path, content, etag, bom }),
    head: (path: string) =>
      request<{ path: string; exists: boolean; content: string; binary: boolean }>("GET", `${base}/git/head${q({ wt, path })}`),
    gitStatus: () => request<GitStatus>("GET", `${base}/git/status${q({ wt })}`),
    gitDiff: (path: string, staged: boolean, origPath?: string | null) =>
      request<GitDiff>("GET", `${base}/git/diff${q({ wt, path, staged, origPath })}`),
    stage: (paths: string[]) => request<GitOpResult>("POST", `${base}/git/stage${q({ wt })}`, { paths }),
    unstage: (paths: string[]) => request<GitOpResult>("POST", `${base}/git/unstage${q({ wt })}`, { paths }),
    commit: (message: string) => request<GitOpResult>("POST", `${base}/git/commit${q({ wt })}`, { message }),
    remote: (op: "fetch" | "pull" | "push") => request<GitOpResult>("POST", `${base}/git/${op}${q({ wt })}`),
    githubToken: () => request<{ token: string | null }>("GET", `${base}/github`),
    setGithubToken: (token: string) => request<{ token: string }>("PUT", `${base}/github`, { token }),
    removeGithubToken: () => request<{ ok: boolean }>("DELETE", `${base}/github`),
    ptyUrl: (cols: number, rows: number) => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      return `${proto}://${location.host}${base}/pty${q({ cols: String(cols), rows: String(rows), wt })}`;
    },
  };
}
