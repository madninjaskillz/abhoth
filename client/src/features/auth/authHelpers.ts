// Pure helpers for the sign-in, setup, account and users pages (no imports: plain node tests run it).

/** The server's password rules: a minimum length and nothing else (no composition rules). */
export const MIN_PASSWORD = 12;
export const MAX_PASSWORD = 1024;

/** Why this password (and its confirmation) can't be used yet, or null. Lengths count characters. */
export function passwordProblem(password: string, confirm?: string): string | null {
  const n = [...password].length;
  if (n === 0) return "Enter a password.";
  if (n < MIN_PASSWORD) return `At least ${MIN_PASSWORD} characters (${MIN_PASSWORD - n} more).`;
  if (n > MAX_PASSWORD) return `At most ${MAX_PASSWORD} characters.`;
  if (confirm !== undefined && confirm !== password) return "The two passwords differ.";
  return null;
}

/** Usernames: 1-64 letters, digits, ".", "_", "-" or "@". */
export function usernameProblem(name: string): string | null {
  const u = name.trim();
  if (!u) return "Enter a username.";
  if (u.length > 64) return "At most 64 characters.";
  if (!/^[\p{L}\p{N}._@-]+$/u.test(u)) return "Letters, digits, . _ - and @ only.";
  return null;
}

/** A typed 2FA code: six digits (spaces allowed), or a recovery code (xxxxx-xxxxx). */
export type CodeInput = { kind: "totp" | "recovery"; value: string; complete: boolean };

/** The authenticator code as typed: digits only, at most six. */
export function totpInput(raw: string): string {
  return raw.replace(/\D/g, "").slice(0, 6);
}

/** A recovery code as typed: lower-case letters and digits, dash after the fifth. */
export function recoveryInput(raw: string): string {
  const chars = raw.toLowerCase().replace(/[^a-z0-9]/g, "").slice(0, 10);
  return chars.length > 5 ? `${chars.slice(0, 5)}-${chars.slice(5)}` : chars;
}

export function codeInput(raw: string, recovery: boolean): CodeInput {
  if (recovery) {
    const value = recoveryInput(raw);
    return { kind: "recovery", value, complete: value.length === 11 };
  }
  const value = totpInput(raw);
  return { kind: "totp", value, complete: value.length === 6 };
}

/** "Firefox on Windows" from a User-Agent; "" when it says nothing useful. */
export function describeAgent(ua: string | null | undefined): string {
  if (!ua) return "";
  const browser =
    /Edg\//.test(ua) ? "Edge" :
    /OPR\//.test(ua) ? "Opera" :
    /Firefox\//.test(ua) ? "Firefox" :
    /Chrome\//.test(ua) ? "Chrome" :
    /Safari\//.test(ua) ? "Safari" :
    /curl\//i.test(ua) ? "curl" : "";
  const os =
    /Windows/.test(ua) ? "Windows" :
    /Android/.test(ua) ? "Android" :
    /iPhone|iPad|iOS/.test(ua) ? "iOS" :
    /Mac OS X|Macintosh/.test(ua) ? "macOS" :
    /Linux/.test(ua) ? "Linux" : "";
  if (browser && os) return `${browser} on ${os}`;
  return browser || os || (ua.length > 40 ? `${ua.slice(0, 40)}…` : ua);
}

/** "3 minutes ago" / "just now" / "" for an ISO time. */
/** The full local date and time of Unix seconds (a tooltip), or "". */
export function when(t: number | null | undefined): string {
  return t ? new Date(t * 1000).toLocaleString() : "";
}

/** How long ago: an ISO time, or Unix seconds (the server's account times). */
export function ago(iso: string | number | null | undefined, now: number = Date.now()): string {
  if (iso === null || iso === undefined || iso === "") return "";
  const t = typeof iso === "number" ? iso * 1000 : Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 60) return "just now";
  const units: [number, string][] = [
    [31536000, "year"],
    [2592000, "month"],
    [604800, "week"],
    [86400, "day"],
    [3600, "hour"],
    [60, "minute"],
  ];
  for (const [size, name] of units) {
    if (s >= size) {
      const n = Math.floor(s / size);
      return `${n} ${name}${n === 1 ? "" : "s"} ago`;
    }
  }
  return "just now";
}

/** Recovery codes as one text block, to copy or save. */
export function codesText(codes: string[], product: string): string {
  return `${product} recovery codes (each works once):\n${codes.join("\n")}\n`;
}

/** An audit entry's event as words: "login.fail" → "login fail". */
export function eventLabel(event: string): string {
  return event.replace(/[._]/g, " ");
}
