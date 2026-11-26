// The calendar's admin calls, through the web server's signed router proxy
// (/api/admin/router/calendar/* -> the router's /admin/api/calendar/*).
import { csrfHeaders } from "@/lib/auth";
import type { CalendarDoc, EntryConfig, Issue, KeyRecord, Occurrence, Overlap, Preview, Priority } from "./calendarModel";

export class CalendarApiError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

async function call<T>(method: string, url: string, body?: unknown): Promise<T> {
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
  let data: unknown;
  try { data = text ? JSON.parse(text) : undefined; } catch { data = undefined; }
  if (!res.ok) {
    const d = (data ?? {}) as Record<string, unknown>;
    throw new CalendarApiError((d.error as string) || res.statusText, res.status);
  }
  return data as T;
}

const C = "/api/admin/router/calendar";
const enc = encodeURIComponent;

export interface SaveResult { rev: string; warnings: Issue[]; overlaps: Overlap[] }
export interface IssuedKey { record: KeyRecord; key: string; note?: string; old_retire_at?: number }
export interface KeyUse { ts: number; key_id: number; event: string; occurrence: string; action: string; decision: string; status: number; detail: string }

export const calendarApi = {
  doc: () => call<CalendarDoc>("GET", C),
  occurrences: (from: number, to: number) => call<{ from: number; to: number; at: number; occurrences: Occurrence[] }>("GET", `${C}/occurrences?from=${from}&to=${to}`),
  preview: (id: string, entry: EntryConfig) => call<Preview>("POST", `${C}/preview`, { id, entry }),
  save: (id: string, entry: EntryConfig, rev: string, reason: string) => call<SaveResult>("PUT", `${C}/entries/${enc(id)}`, { entry, rev, reason }),
  remove: (id: string, rev: string) => call<SaveResult>("DELETE", `${C}/entries/${enc(id)}?rev=${enc(rev)}`),
  occurrence: (occ: string, action: "skip" | "cancel" | "restore", reason: string) => call<{ ok: boolean }>("POST", `${C}/occurrences/${enc(occ)}/${action}`, { reason }),
  issueKey: (event: string) => call<IssuedKey>("POST", `${C}/keys/${enc(event)}/issue`),
  rotateKey: (event: string) => call<IssuedKey>("POST", `${C}/keys/${enc(event)}/rotate`),
  revokeKey: (id: number) => call<{ ok: boolean }>("DELETE", `${C}/keys/${id}`),
  uses: (event: string) => call<KeyUse[]>("GET", `${C}/uses/${enc(event)}`),
  importIcs: (text: string, effects: unknown[], priority: Priority, apply: boolean) =>
    call<{ entries: string[]; issues: Issue[]; toml: string }>("POST", `${C}/import-ics`, { text, effects, priority, apply }),
};
