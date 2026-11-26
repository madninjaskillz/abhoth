// The router admin page's session (D151), plain TypeScript for node --test.

/** What GET <page>/session answers (the router, or the Abhoth app's proxy when embedded). */
export interface RouterSession {
  available: boolean;
  signedIn: boolean;
  admin: boolean;
  principal?: string | null;
  /** session (cookie), key (bearer), loopback, web (the app's signed proxy), app (embedded). */
  via?: string;
  csrfHeader?: string;
  csrf?: string | null;
  message?: string | null;
  version?: string;
  name?: string;
  controlPlane?: boolean;
  localKeys?: boolean;
  hasKeys?: boolean;
  linked?: boolean;
  embedded?: boolean;
}

/** A URL next to the page (`session`, `login`, `api`): relative, so any proxy prefix is kept. */
export function pageUrl(rel: string, base: string = typeof document !== "undefined" ? document.baseURI : "http://localhost/admin/"): string {
  const u = new URL(rel, base);
  return u.pathname + u.search;
}
