// Pure helpers for the "Clone from GitHub" wizard (no imports, so plain `node --test` runs them).

/** "acme/my-repo" → "my-repo" (the suggested folder name). */
export function repoFolderName(fullName: string): string {
  const name = fullName.trim().replace(/\/+$/, "").split("/").pop() ?? "";
  return name.replace(/\.git$/i, "") || "repository";
}

/** "3 days ago", "just now", "2 years ago": how long since a repository was last pushed to. */
export function timeAgo(iso: string | null | undefined, now: number = Date.now()): string {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 60) return "just now";
  const units: [number, string][] = [
    [60 * 60 * 24 * 365, "year"],
    [60 * 60 * 24 * 30, "month"],
    [60 * 60 * 24 * 7, "week"],
    [60 * 60 * 24, "day"],
    [60 * 60, "hour"],
    [60, "minute"],
  ];
  for (const [size, unit] of units) {
    const n = Math.floor(s / size);
    if (n >= 1) return `${n} ${unit}${n === 1 ? "" : "s"} ago`;
  }
  return "just now";
}

/** "31–60 of 131" for a page of results (1-based page); "No repositories" when empty. */
export function pageRange(page: number, perPage: number, total: number): string {
  if (total <= 0) return "No repositories";
  const from = (page - 1) * perPage + 1;
  const to = Math.min(page * perPage, total);
  return `${from}–${to} of ${total}`;
}

export function pageCount(perPage: number, total: number): number {
  return Math.max(1, Math.ceil(total / Math.max(1, perPage)));
}

export type DeviceState = "pending" | "done" | "expired" | "denied" | "error";

/** How long to wait before the next poll: GitHub's interval (in seconds, at least 1), as milliseconds. */
export function pollDelayMs(interval: number | null | undefined): number {
  const s = typeof interval === "number" && Number.isFinite(interval) ? interval : 5;
  return Math.max(1, s) * 1000;
}

/** What to tell the user when a device flow ends without signing in. */
export function deviceEndMessage(state: DeviceState, error?: string | null): string {
  if (state === "expired") return error || "The code expired before it was entered. Start again.";
  if (state === "denied") return error || "The sign-in was cancelled on GitHub.";
  if (state === "error") return error || "GitHub sign-in failed.";
  return "";
}

/**
 * "owner/repo" from what was typed: owner/repo, owner/repo.git, or a github.com address
 * (https://github.com/owner/repo, git@github.com:owner/repo.git); `null` when it isn't one.
 */
export function ownerRepo(typed: string): string | null {
  let t = typed.trim().replace(/\/+$/, "").replace(/\.git$/i, "");
  const url = /^(?:https?:\/\/(?:www\.)?github\.com\/|git@github\.com:)([^/]+\/[^/]+)$/i.exec(t);
  if (url) t = url[1];
  if (!/^[A-Za-z0-9][A-Za-z0-9-]{0,38}\/[A-Za-z0-9._-]{1,100}$/.test(t) || t.includes("..")) return null;
  return t;
}
