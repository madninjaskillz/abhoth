// REST calls to the server. Auth is the HttpOnly cookie set when the page was opened with ?token=.

export class ApiError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

async function request<T>(method: string, url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    credentials: "same-origin",
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
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
    throw new ApiError(res.status === 401 ? "Not signed in: open the URL OpenOrca printed (with ?token=)." : err, res.status);
  }
  return data as T;
}

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
}

export const api = {
  info: () => request<ServerInfo>("GET", "/api/info"),
  gitStatus: () => request<GitStatus>("GET", "/api/git/status"),
  gitDiff: (path: string, staged: boolean, origPath?: string | null) =>
    request<GitDiff>(
      "GET",
      `/api/git/diff?path=${encodeURIComponent(path)}&staged=${staged}` +
        (origPath ? `&origPath=${encodeURIComponent(origPath)}` : ""),
    ),
  stage: (paths: string[]) => request<GitOpResult>("POST", "/api/git/stage", { paths }),
  unstage: (paths: string[]) => request<GitOpResult>("POST", "/api/git/unstage", { paths }),
  commit: (message: string) => request<GitOpResult>("POST", "/api/git/commit", { message }),
};
