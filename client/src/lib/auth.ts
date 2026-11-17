// Sign-in mode (openorca --hostui-login): session cookie + CSRF header. Token mode has no /api/auth.

export type AuthStage = "signedOut" | "totp" | "enroll" | "ok";

export interface AuthState {
  mode: "login" | "token";
  stage: AuthStage;
  user?: string | null;
  csrf?: string | null;
  totp?: boolean;
  totpRequired?: boolean;
  idleMinutes?: number;
}

let current: AuthState = { mode: "token", stage: "ok" };

export const authState = () => current;

/** Header for state-changing requests (sign-in mode only). */
export function csrfHeaders(): Record<string, string> {
  return current.mode === "login" && current.csrf ? { "X-OpenOrca-CSRF": current.csrf } : {};
}

export async function loadAuth(): Promise<AuthState> {
  try {
    const res = await fetch("/api/auth/state", { credentials: "same-origin", cache: "no-store" });
    if (res.ok) current = (await res.json()) as AuthState;
    else current = { mode: "token", stage: "ok" };
  } catch {
    current = { mode: "token", stage: "ok" };
  }
  return current;
}

async function post<T>(url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", ...csrfHeaders() },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = (await res.json().catch(() => ({}))) as T & { error?: string };
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}

export async function signIn(username: string, password: string): Promise<AuthState> {
  current = await post<AuthState>("/api/auth/login", { username, password });
  return current;
}

export async function submitCode(code: string): Promise<AuthState> {
  current = await post<AuthState>("/api/auth/totp", { code });
  return current;
}

export async function startEnroll(): Promise<AuthState> {
  current = await post<AuthState>("/api/auth/enroll/start");
  return current;
}

export async function enrollInfo(): Promise<{ qr: string; secret: string; uri: string }> {
  const res = await fetch("/api/auth/enroll", { credentials: "same-origin", cache: "no-store" });
  if (!res.ok) throw new Error("Couldn't start 2FA setup.");
  return res.json();
}

export async function signOut(): Promise<void> {
  await post("/api/auth/logout").catch(() => undefined);
  location.reload();
}

/** A request came back 401 in sign-in mode: the session ended, show the sign-in page. */
export function sessionEnded() {
  if (current.mode === "login") location.reload();
}
