// Accounts: a server-side session in an HttpOnly cookie, plus its CSRF header on every
// state-changing request. Scripts use API keys (Authorization: Bearer) instead.

export type AuthStage = "signedOut" | "totp" | "enroll" | "ok";

export interface AuthState {
  mode: "login";
  /** No users yet: the first-run setup page. */
  setup: boolean;
  /** The setup page must ask for the code the server printed (no setup cookie from the launch URL). */
  setupCodeNeeded: boolean;
  /** Setup on an exposed server for the desktop's local admin: it only needs a password. */
  setupUser?: string | null;
  stage: AuthStage;
  user: string | null;
  userId: number | null;
  admin: boolean;
  totp: boolean;
  totpRequired: boolean;
  csrf: string | null;
  idleMinutes: number;
  desktop: boolean;
  /** Set when /api/auth/state couldn't be read. */
  error?: string;
  /** After a forced 2FA setup at sign-in: shown once. */
  recoveryCodes?: string[];
}

const signedOutState = (error?: string): AuthState => ({
  mode: "login",
  setup: false,
  setupCodeNeeded: false,
  stage: "signedOut",
  user: null,
  userId: null,
  admin: false,
  totp: false,
  totpRequired: false,
  csrf: null,
  idleMinutes: 0,
  desktop: false,
  error,
});

let current: AuthState = signedOutState();

export const authState = () => current;

/** Signed in as an admin (server settings, users, bench, GitHub App). */
export const isAdmin = () => current.stage === "ok" && current.admin;

/** Header for state-changing requests. */
export function csrfHeaders(): Record<string, string> {
  return current.csrf ? { "X-OpenOrca-CSRF": current.csrf } : {};
}

export async function loadAuth(): Promise<AuthState> {
  try {
    const res = await fetch("/api/auth/state", { credentials: "same-origin", cache: "no-store" });
    if (res.ok) current = { ...signedOutState(), ...((await res.json()) as Partial<AuthState>), mode: "login" };
    else current = signedOutState(`The server answered ${res.status} ${res.statusText}.`);
  } catch (e) {
    current = signedOutState(`Can't reach the server (${e instanceof Error ? e.message : String(e)}).`);
  }
  return current;
}

export class AuthError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

async function call<T>(method: string, url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    credentials: "same-origin",
    cache: "no-store",
    headers: {
      ...(body === undefined ? {} : { "Content-Type": "application/json" }),
      ...(method === "GET" ? {} : csrfHeaders()),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = (await res.json().catch(() => ({}))) as T & { error?: string };
  if (!res.ok) {
    if (res.status === 401 && current.stage === "ok") sessionEnded();
    throw new AuthError(data?.error || (res.status === 429 ? "Too many attempts; wait a little." : res.statusText || `Error ${res.status}`), res.status);
  }
  return data;
}

const post = <T>(url: string, body?: unknown) => call<T>("POST", url, body);

async function setState(p: Promise<AuthState>): Promise<AuthState> {
  const s = await p;
  current = { ...signedOutState(), ...s, mode: "login" };
  return current;
}

// ---- sign-in and setup ----

export const setupAdmin = (username: string, password: string, code?: string) =>
  setState(post<AuthState>("/api/auth/setup", { username, password, ...(code ? { code } : {}) }));

export const signIn = (username: string, password: string) => setState(post<AuthState>("/api/auth/login", { username, password }));

/** A 6-digit authenticator code or a recovery code. */
export const submitCode = (code: string) => setState(post<AuthState>("/api/auth/totp", { code }));

export async function enrollInfo(): Promise<TotpSetup> {
  return call<TotpSetup>("GET", "/api/auth/enroll");
}

export async function signOut(): Promise<void> {
  await post("/api/auth/logout").catch(() => undefined);
  location.assign("/");
}

/** A request came back 401: the session ended, show the sign-in page. */
export function sessionEnded() {
  location.reload();
}

// ---- the signed-in account ----

export interface TotpSetup {
  qr: string;
  secret: string;
  uri: string;
}

export interface Account {
  id: number;
  username: string;
  admin: boolean;
  totp: boolean;
  /** The desktop's local admin, without a password yet (signs in only through the desktop). */
  localOnly?: boolean;
  created: number;
  lastLogin: number | null;
  recoveryCodesLeft: number;
}

export interface ApiKey {
  id: number;
  name: string;
  prefix: string;
  created: number;
  lastUsed: number | null;
}

export interface NewApiKey extends ApiKey {
  /** Shown once. */
  key: string;
}

export interface WebSessionInfo {
  id: number;
  created: number;
  lastSeen: number;
  ip: string | null;
  userAgent: string | null;
  current: boolean;
  desktop: boolean;
}

type Ok = { ok: true };
type Codes = { recoveryCodes: string[] };

export const accountApi = {
  get: () => call<Account>("GET", "/api/account"),
  changePassword: (current: string, password: string) => post<Ok>("/api/account/password", { current, password }),
  totpStart: () => post<TotpSetup>("/api/account/totp/start"),
  totpConfirm: (code: string) => post<Codes>("/api/account/totp/confirm", { code }),
  totpDisable: (password: string) => post<Ok>("/api/account/totp/disable", { password }),
  recoveryCodes: (password: string) => post<Codes>("/api/account/recovery-codes", { password }),
  keys: () => call<ApiKey[]>("GET", "/api/account/keys"),
  createKey: (name: string) => post<NewApiKey>("/api/account/keys", { name }),
  revokeKey: (id: number) => call<Ok>("DELETE", `/api/account/keys/${id}`),
  sessions: () => call<WebSessionInfo[]>("GET", "/api/account/sessions"),
  revokeSession: (id: number) => call<Ok>("DELETE", `/api/account/sessions/${id}`),
  revokeAll: () => post<Ok>("/api/account/sessions/revoke-all"),
};

// ---- admin ----

export interface UserRow {
  id: number;
  username: string;
  admin: boolean;
  disabled: boolean;
  totp: boolean;
  created: number;
  lastLogin: number | null;
  projects: number;
}

export interface AuditEntry {
  id: number;
  time: number;
  event: string;
  actor: string | null;
  target: string | null;
  ip: string | null;
  detail: Record<string, unknown> | null;
}

export const adminApi = {
  users: () => call<UserRow[]>("GET", "/api/admin/users"),
  create: (username: string, password: string, admin: boolean) => post<UserRow>("/api/admin/users", { username, password, admin }),
  update: (id: number, patch: { disabled?: boolean; admin?: boolean }) => call<UserRow>("PATCH", `/api/admin/users/${id}`, patch),
  resetPassword: (id: number, password: string) => post<Ok>(`/api/admin/users/${id}/password`, { password }),
  resetTotp: (id: number) => post<Ok>(`/api/admin/users/${id}/totp/reset`),
  remove: (id: number) => call<Ok>("DELETE", `/api/admin/users/${id}`),
  audit: (before?: number, limit = 200) =>
    call<AuditEntry[]>("GET", `/api/admin/audit?limit=${limit}${before !== undefined ? `&before=${before}` : ""}`),
};
