// REST calls to the server. Auth is an HttpOnly cookie: the token's (opened with ?token=) or, in
// sign-in mode, the session's, plus its CSRF header on state-changing requests.
import { authState, csrfHeaders, sessionEnded } from "@/lib/auth";

export class ApiError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

/** Uploads prompt attachments (multipart) into the project's .orca/attachments folder. */
export async function uploadAttachments(pid: string, files: File[]): Promise<{ name: string; path: string; kind: "image" | "file"; size: number }[]> {
  const form = new FormData();
  files.forEach((f) => form.append("files", f, f.name));
  const res = await fetch(`/api/projects/${encodeURIComponent(pid)}/attachments`, {
    method: "POST",
    credentials: "same-origin",
    headers: csrfHeaders(),
    body: form,
  });
  const text = await res.text();
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = undefined; }
  if (!res.ok) {
    if (res.status === 401) sessionEnded();
    throw new ApiError((data as { error?: string } | undefined)?.error || res.statusText, res.status);
  }
  return data as { name: string; path: string; kind: "image" | "file"; size: number }[];
}

/** The URL that serves a project file (images, PDFs, media inline; anything else downloads). */
export function rawUrl(pid: string, path: string, wt?: string | null): string {
  const qs = new URLSearchParams({ path });
  if (wt) qs.set("wt", wt);
  return `/api/projects/${encodeURIComponent(pid)}/raw?${qs}`;
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
  /** A merge is in progress (conflicted files are in files, conflicted: true). */
  merging?: boolean;
  mergeBranch?: string | null;
}

export interface GitBranch {
  name: string;
  remote: boolean;
  current: boolean;
  upstream?: string | null;
  head: string;
  subject: string;
  when: string;
  /** Checked out in this worktree folder (local branches). */
  worktree?: string | null;
}

export interface GitBranches {
  current?: string | null;
  detached: boolean;
  local: GitBranch[];
  remote: GitBranch[];
}

export interface GitStash {
  index: number;
  message: string;
  when: string;
}

export interface GitDiff {
  path: string;
  original: string;
  modified: string;
  binary: boolean;
  tooLarge: boolean;
}

/** One /config setting, as the server describes it (a secret's value never comes). */
export interface Setting {
  key: string;
  label: string;
  type: "bool" | "int" | "float" | "text" | "url" | "secret" | "list";
  description: string;
  value: boolean | number | string | string[] | null;
  display: string;
  default: boolean | number | string | string[] | null;
  defaultDisplay: string;
  /** Secrets: whether one is set. */
  set?: boolean | null;
  modified: boolean;
  /** Applies to the running session at once; otherwise to sessions started afterwards. */
  live: boolean;
  dangerous: boolean;
  min?: number | null;
  max?: number | null;
  minExclusive?: boolean;
  blankMeans?: string | null;
  /** The profile that sets this key (project scope: the running session's; server scope: the default profile). */
  profile?: string | null;
}

export interface SettingsDoc {
  sections: { name: string; settings: Setting[] }[];
  configFile?: string;
  /** Project scope: the profile the running session has; server scope: the default profile, when its keys are known. */
  profile?: string | null;
}

export interface SettingResult {
  ok: boolean;
  key: string;
  old?: string;
  new?: string;
  live?: boolean;
  error?: string;
}

/** A settings change: its result whether it worked or not. */
async function settingOp(url: string, body: unknown): Promise<SettingResult> {
  try {
    return await request<SettingResult>("PUT", url, body);
  } catch (err) {
    const data = (err as { data?: unknown }).data as Partial<SettingResult> | undefined;
    if (data && typeof data.ok === "boolean") return { ok: false, key: data.key ?? "", error: data.error ?? "Refused." };
    throw err;
  }
}

/** The settings page's two scopes: the server's config.json, or one project's running session. */
export const settingsApi = {
  get: (pid: string | null) => request<SettingsDoc>("GET", pid ? `/api/projects/${pid}/settings` : "/api/settings"),
  set: (pid: string | null, key: string, value: unknown, reset = false) =>
    settingOp(pid ? `/api/projects/${pid}/settings` : "/api/settings", { key, value, reset }),
};

export interface GitOpResult {
  ok: boolean;
  output: string;
  error: string;
  /** Why it failed, when there's a next step to offer: "dirty", "unmerged", "conflicts". */
  code?: string | null;
}

/** A git change: its result whether it worked or not (git's refusals are results, not exceptions). */
async function gitOp(method: string, url: string, body?: unknown): Promise<GitOpResult> {
  try {
    return await request<GitOpResult>(method, url, body);
  } catch (e) {
    const data = (e as { data?: unknown }).data as Partial<GitOpResult> | undefined;
    if (data && typeof data.ok === "boolean") return { ok: data.ok, output: data.output ?? "", error: data.error ?? "", code: data.code };
    throw e;
  }
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
  /** The inference server's profiles API is configured (profile pickers, server layout). */
  profiles?: boolean;
}

/** One card of a server layout: which engine, how many slots, which model file. */
export interface LayoutCard {
  engine: string;
  slots: number;
  model?: string | null;
}

export interface LayoutDefinition {
  title?: string | null;
  cards: Record<string, LayoutCard>;
}

export interface ProfileInfo {
  title?: string | null;
  description?: string | null;
  /** The server layout it needs; null = none (a bridge engine). */
  layout?: string | null;
  /** Dotted config overrides ("agent.peerMode": true, ...). */
  client: Record<string, unknown>;
  /** Its layout is live (or it needs none). */
  fits: boolean;
  /** "needs restore (≈24 GB, ~6 min)" when its model files are offline. */
  restore?: string | null;
}

export interface LiveLayout {
  layout?: string | null;
  title?: string | null;
  healthy?: boolean;
  cards?: Record<string, { engine?: string | null; slots?: number | null; model?: string | null; healthy?: boolean | null; detail?: string | null }>;
  leases?: { slot: string; who?: string | null }[];
  quietWindow?: boolean;
  job?: { id?: string | null; state?: string | null; layout?: string | null } | null;
}

export interface ProfileList {
  available: boolean;
  error?: string | null;
  offline: boolean;
  fetchedAt?: string | null;
  layouts?: Record<string, LayoutDefinition> | null;
  profiles?: Record<string, ProfileInfo> | null;
  live?: LiveLayout | null;
  /** Each project's chosen profile (null = the config's own settings). */
  projects: Record<string, string | null>;
}

export interface SwitchPlan {
  profile?: string | null;
  layout?: string | null;
  live?: string | null;
  fits: boolean;
  estimateSeconds: number;
  effect?: string | null;
  refusals: string[];
  restore?: { text?: string | null } | null;
}

export interface SwitchJob {
  id?: string | null;
  state?: string | null;
  layout?: string | null;
  error?: string | null;
  seconds?: number | null;
  already?: boolean;
  live?: string | null;
  steps?: { t: number; msg: string }[];
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
  /** An inference profile: its settings under the arm's, its server layout switched to first. */
  profile?: string | null;
}

/** One model endpoint a targeted suite's runs go to (one run at a time each). The key is named by env var, never given. */
export interface BenchTarget {
  name: string;
  endpoint: string;
  model?: string | null;
  apiKeyEnv?: string | null;
}

export interface BenchSuite {
  name: string;
  arms: BenchArm[];
  tasks: string[];
  reps: number;
  timeoutSeconds?: number | null;
  description?: string | null;
  /** The arms' profile when they name none. */
  profile?: string | null;
  /** Targeted suite: runs go straight to these endpoints, one per target, targets in parallel. */
  targets?: BenchTarget[] | null;
  /** "pool" (default: whichever target is free) or "each" (every run on every target). */
  targetMode?: string | null;
}

/** Requests and tokens a run has used so far (or in all, once it ended). */
export interface BenchLive {
  requests: number;
  promptTokens: number;
  cachedTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  contentTokens: number;
  toolTokens: number;
}

/** A line of the summary table: every run of a task on an arm (and a target, when asked by target). */
export interface BenchSummaryRow {
  task: string;
  arm: string;
  target?: string | null;
  runs: number;
  scored: number;
  passes: number;
  medianWallSeconds?: number | null;
  medianSteps?: number | null;
  medianPromptTokens?: number | null;
  medianOutputTokens?: number | null;
  medianReasoningTokens?: number | null;
  medianToolTokens?: number | null;
  medianContentTokens?: number | null;
}

/** The live stream's status line. */
export interface BenchStatus {
  paused: boolean;
  gate?: string | null;
  running: number;
  queued: number;
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
  profile?: string | null;
  /** The "Queue suite" click the run came from: one grid. */
  batch?: string | null;
  target?: string | null;
  /** Seconds since it started (to its end, once ended). */
  elapsed?: number | null;
  wallSeconds?: number | null;
  pass?: boolean | null;
  testsPassed?: number | null;
  testsTotal?: number | null;
  model?: string | null;
  live?: BenchLive | null;
}

export interface BenchState {
  paused: boolean;
  gate?: string | null;
  /** The first running run (kept for the status pill); running = all of them. */
  current?: BenchRunInfo | null;
  running?: BenchRunInfo[];
  queue: BenchRunInfo[];
  recent: BenchRunInfo[];
  suites: BenchSuite[];
}

/** A results.jsonl row. The schema, with what each field means, is BenchResultRow.cs (server). */
export interface BenchResult {
  id: string;
  suite: string;
  arm: string;
  lead_mode: string;
  sets?: string[];
  task: string;
  rep: number;
  /** The inference profile the arm ran; layout = the server layout live at the start. */
  profile?: string | null;
  layout?: string | null;
  /** Resolved inference settings (agent, slots, thinking, models; no keys). */
  settings?: Record<string, unknown> | null;
  batch?: string | null;
  target?: string | null;
  target_endpoint?: string | null;
  model?: string | null;
  status: string;
  pass?: boolean;
  reason?: string;
  tests_passed?: number;
  tests_total?: number;
  timeout?: boolean;
  quiet_window_stop?: boolean;
  note?: string | null;
  started?: string;
  ended?: string | null;
  wall_s?: number;
  requests?: number;
  prompt_tokens?: number;
  cached_tokens?: number;
  output_tokens?: number;
  reasoning_tokens?: number;
  /** The rest of the output: visible text, and tool calls with their arguments. */
  content_tokens?: number;
  tool_tokens?: number;
  decode_tps?: number | null;
  lead_tps?: number | null;
  helpers?: number;
  helper_routes?: string[] | null;
  slots?: string[] | null;
  /** Decode tok/s per model copy ("A", "B"). */
  copy_tps?: Record<string, number> | null;
  bridge_turns?: number | null;
  cost_usd?: number | null;
  mean_w?: number | null;
  orca_version?: string | null;
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
  folders: { name: string; path: string; isRepo: boolean; repoError?: string | null }[];
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
  benchStart: (suite: string) => request<{ queued: number; batch: string }>("POST", "/api/bench/start", { suite }),
  benchTasks: () => request<{ name: string; timeout?: number }[]>("GET", "/api/bench/tasks"),
  benchSaveSuite: (suite: BenchSuite, opts: { overwrite?: boolean; start?: boolean } = {}) =>
    request<{ suite: BenchSuite; queued?: number; batch?: string }>("POST", "/api/bench/suites", { suite, ...opts }),
  benchCancelRun: (id: string) => request<{ ok: boolean }>("POST", `/api/bench/runs/${encodeURIComponent(id)}/cancel`),
  benchCancelBatch: (id: string) => request<{ ok: boolean; cancelled: number }>("POST", `/api/bench/batches/${encodeURIComponent(id)}/cancel`),
  benchSummary: (batch: string, byTarget: boolean) =>
    request<BenchSummaryRow[]>("GET", `/api/bench/summary?batch=${encodeURIComponent(batch)}${byTarget ? "&byTarget=true" : ""}`),
  benchLogTail: async (id: string, chars: number) => {
    const res = await fetch(`/api/bench/runs/${encodeURIComponent(id)}/logs?tail=${chars}`, { credentials: "same-origin" });
    if (res.status === 404) return "";
    if (!res.ok) throw new ApiError(res.statusText, res.status);
    return res.text();
  },
  benchControl: (what: "pause" | "resume" | "stop") => request<{ ok: boolean }>("POST", `/api/bench/${what}`),
  benchOutput: async (id: string) => {
    const res = await fetch(`/api/bench/runs/${encodeURIComponent(id)}/output`, { credentials: "same-origin" });
    return res.ok ? res.text() : "(no checker output for this run)";
  },
  browse: (path?: string) => request<FolderListing>("GET", `/api/fs/browse${q({ path })}`),
  clone: (url: string, parent: string, name?: string, token?: string) =>
    request<CloneStatus>("POST", "/api/projects/clone", { url, parent, name, token: token || undefined }),
  validateGithub: (body: { token?: string; pid?: string; url?: string }) => request<GitHubCheck>("POST", "/api/github/validate", body),
  benchLogs: async (id: string) => {
    const res = await fetch(`/api/bench/runs/${encodeURIComponent(id)}/logs`, { credentials: "same-origin" });
    return res.ok ? res.text() : "(no logs for this run)";
  },
  profiles: () => request<ProfileList>("GET", "/api/profiles"),
  profilePlan: (profile: string) => request<SwitchPlan>("GET", `/api/profiles/plan${q({ profile })}`),
  profileSwitch: (profile: string) => request<SwitchJob>("POST", "/api/profiles/switch", { profile }),
  profileJob: (id: string) => request<SwitchJob>("GET", `/api/profiles/jobs/${encodeURIComponent(id)}`),
  setProjectProfile: (pid: string, profile: string | null) =>
    request<{ ok: boolean; profile: string | null; message?: string | null }>("POST", `/api/projects/${encodeURIComponent(pid)}/profile`, { profile }),
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
    /** A signed, expiring URL that serves an HTML file's folder to the sandboxed preview frame. */
    preview: (path: string) =>
      request<{ url: string; base: string; dir: string; expires: string }>("POST", `${base}/preview${q({ wt })}`, { path }),
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
    branches: () => request<GitBranches>("GET", `${base}/git/branches${q({ wt })}`),
    checkout: (branch: string, stash = false) => gitOp("POST", `${base}/git/checkout${q({ wt })}`, { branch, stash }),
    createBranch: (name: string, from: string | null, checkout: boolean) =>
      gitOp("POST", `${base}/git/branch${q({ wt })}`, { name, from: from || null, checkout }),
    renameBranch: (name: string, newName: string) => gitOp("POST", `${base}/git/branch/rename${q({ wt })}`, { name, newName }),
    deleteBranch: (name: string, force = false) => gitOp("POST", `${base}/git/branch/delete${q({ wt })}`, { name, force }),
    publish: () => gitOp("POST", `${base}/git/publish${q({ wt })}`),
    merge: (branch: string) => gitOp("POST", `${base}/git/merge${q({ wt })}`, { branch }),
    mergeAbort: () => gitOp("POST", `${base}/git/merge/abort${q({ wt })}`),
    mergeContinue: () => gitOp("POST", `${base}/git/merge/continue${q({ wt })}`),
    resolve: (paths: string[]) => gitOp("POST", `${base}/git/merge/resolve${q({ wt })}`, { paths }),
    stashes: () => request<GitStash[]>("GET", `${base}/git/stashes${q({ wt })}`),
    stash: (message?: string) => gitOp("POST", `${base}/git/stash${q({ wt })}`, { message: message || null }),
    stashOp: (op: "apply" | "pop" | "drop", index: number) => gitOp("POST", `${base}/git/stash/${op}${q({ wt })}`, { index }),
    addWorktree: (branch: string, newBranch: boolean, from?: string | null) =>
      gitOp("POST", `${base}/worktrees`, { branch, newBranch, from: from || null }),
    removeWorktree: (path: string, force = false) => gitOp("DELETE", `${base}/worktrees${q({ path, force: force || undefined })}`),
    githubToken: () => request<{ token: string | null }>("GET", `${base}/github`),
    setGithubToken: (token: string) => request<{ token: string }>("PUT", `${base}/github`, { token }),
    removeGithubToken: () => request<{ ok: boolean }>("DELETE", `${base}/github`),
    ptyUrl: (cols: number, rows: number) => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      return `${proto}://${location.host}${base}/pty${q({ cols: String(cols), rows: String(rows), wt })}`;
    },
  };
}
