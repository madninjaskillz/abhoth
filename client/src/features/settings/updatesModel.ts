// Settings → Updates' model (plain TS, no DOM): the update service's state in words.

export type UpdatePhase = "unavailable" | "idle" | "checking" | "downloading" | "ready" | "upToDate" | "error";

export interface UpdateState {
  phase: UpdatePhase;
  current: string;
  channel: string;
  available?: string | null;
  notesUrl?: string | null;
  progress?: number | null;
  /** Unix seconds. */
  lastCheck?: number | null;
  lastResult?: string | null;
  error?: string | null;
  by?: "you" | "automatic" | string | null;
  reason?: string | null;
}

/** Whether the page should poll (something is under way). */
export function busy(s: UpdateState | undefined): boolean {
  return !!s && (s.phase === "checking" || s.phase === "downloading");
}

/** "just now", "5 minutes ago", "3 hours ago", "2 days ago"; "never". */
export function ago(unix: number | null | undefined, nowMs = Date.now()): string {
  if (!unix) return "never";
  const s = Math.max(0, Math.round(nowMs / 1000 - unix));
  if (s < 45) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m} minute${m === 1 ? "" : "s"} ago`;
  const h = Math.round(m / 60);
  if (h < 36) return `${h} hour${h === 1 ? "" : "s"} ago`;
  const d = Math.round(h / 24);
  return `${d} day${d === 1 ? "" : "s"} ago`;
}

/** The status line under the version. */
export function statusLine(s: UpdateState): string {
  switch (s.phase) {
    case "unavailable":
      return s.reason ?? "Updates aren't available in this copy.";
    case "checking":
      return "Checking GitHub for a new release…";
    case "downloading":
      return `Downloading ${s.available ?? "the update"}… ${s.progress ?? 0}%`;
    case "ready":
      return `${s.available} is downloaded. Restart to finish updating.`;
    case "upToDate":
      return `You're on the latest release (${s.current}).`;
    case "error":
      return s.error ?? "The last check failed.";
    default:
      return "Not checked since the app started.";
  }
}

/** Whether "Check for updates now" can be pressed, and why not. */
export function canCheck(s: UpdateState): { ok: boolean; why?: string } {
  if (s.phase === "unavailable") return { ok: false, why: "Not an installed copy" };
  if (busy(s)) return { ok: false, why: "A check is under way" };
  if (s.phase === "ready") return { ok: false, why: "An update is ready" };
  return { ok: true };
}
