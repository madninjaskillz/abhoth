// The router's own admin pages (D151): served by `abhoth router` at /admin/ (no Abhoth web server
// needed), and embedded in the Abhoth app through its proxy. Sign-in with an admin key gives a
// session cookie; the panes are the app's router panes and forms, pointed at this router.
import mascot128 from "@/assets/brand/abhoth-mascot-128.webp";
import { lazy, Suspense, useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Activity, Boxes, Gauge, HardDrive, KeyRound, Loader2, LogOut, Route, ServerCog, Settings2, Store } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { RouterActivityPane, RouterOverviewPane, RouterRoutesPane } from "../RouterPanes";
import { setRouterTransport } from "../routerApi";
import { AdminKeys } from "../forms/AdminKeys";
import { EngineStore } from "../forms/EngineStore";
import { GatewayForms } from "../forms/GatewayForms";
import { InventoryForms } from "../forms/InventoryForms";
import { RouterSetupForms } from "../forms/SetupForms";
import { pageUrl, type RouterSession } from "./session";

const ConfigEditor = lazy(() => import("../ConfigEditor"));

async function loadSession(): Promise<RouterSession> {
  const r = await fetch(pageUrl("session"), { credentials: "same-origin", cache: "no-store" });
  if (!r.ok) throw new Error(`The router answered ${r.status}.`);
  return (await r.json()) as RouterSession;
}

const TABS = [
  { id: "overview", label: "Overview", icon: Gauge },
  { id: "inventory", label: "Inventory", icon: HardDrive },
  { id: "store", label: "Engine store", icon: Store },
  { id: "setup", label: "Servers & layouts", icon: ServerCog },
  { id: "models", label: "Model list", icon: Boxes },
  { id: "routes", label: "Routes", icon: Route },
  { id: "advanced", label: "Advanced", icon: Settings2 },
  { id: "activity", label: "Jobs & audit", icon: Activity },
  { id: "keys", label: "Admin keys", icon: KeyRound },
] as const;
type TabId = (typeof TABS)[number]["id"];

export function RouterAdminApp() {
  const qc = useQueryClient();
  const session = useQuery({ queryKey: ["router-session"], queryFn: loadSession, refetchOnWindowFocus: false, retry: false });
  const s = session.data;
  // Point the shared router API at this router with this session's CSRF header.
  useEffect(() => {
    if (!s) return;
    setRouterTransport({
      base: pageUrl("api").replace(/\/$/, ""),
      headers: () => (s.csrf && s.csrfHeader ? { [s.csrfHeader]: s.csrf } : {}),
      unauthorized: () => void qc.invalidateQueries({ queryKey: ["router-session"] }),
    });
  }, [s, qc]);
  if (session.isLoading) return <Center><Loader2 className="size-5 animate-spin text-muted-foreground" /></Center>;
  if (session.isError || !s) return <Center><p className="text-sm text-destructive">{String(session.error ?? "No answer from the router.")}</p></Center>;
  if (!s.available) return <Center><Unavailable s={s} /></Center>;
  if (!s.admin) return <Center><SignIn s={s} onDone={() => void qc.invalidateQueries({ queryKey: ["router-session"] })} /></Center>;
  return <Shell s={s} onSignedOut={() => void qc.invalidateQueries({ queryKey: ["router-session"] })} />;
}

function Center({ children }: { children: React.ReactNode }) {
  return <div className="flex min-h-full items-center justify-center p-6">{children}</div>;
}

function Brand({ s }: { s: RouterSession }) {
  return (
    <div className="flex items-center gap-2">
      <img src={mascot128} alt="" className="size-7" onError={(e) => ((e.target as HTMLImageElement).style.display = "none")} />
      <div>
        <div className="text-sm font-semibold leading-tight">{s.name || "Abhoth router"}</div>
        <div className="text-[11px] text-muted-foreground">admin · v{s.version}</div>
      </div>
    </div>
  );
}

function Unavailable({ s }: { s: RouterSession }) {
  return (
    <div className="max-w-md space-y-3 rounded-lg border bg-card p-6" data-testid="router-admin-off">
      <Brand s={s} />
      <p className="text-sm">{s.message}</p>
    </div>
  );
}

function SignIn({ s, onDone }: { s: RouterSession; onDone: () => void }) {
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(s.message ?? null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const r = await fetch(pageUrl("login"), {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ key }),
      });
      const d = (await r.json().catch(() => ({}))) as { error?: string };
      if (!r.ok) throw new Error(d.error || `The router answered ${r.status}.`);
      setKey("");
      onDone();
    } catch (x) {
      setError(x instanceof Error ? x.message : String(x));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form onSubmit={submit} className="w-full max-w-sm space-y-4 rounded-lg border bg-card p-6 shadow-sm" data-testid="router-signin">
      <Brand s={s} />
      <div className="space-y-1">
        <h1 className="text-base font-semibold">Sign in</h1>
        <p className="text-xs text-muted-foreground">
          With an admin key (abk_…){s.linked ? ": one of this router's own, or an admin's key from the linked Abhoth server" : " made on this router"}.
        </p>
      </div>
      <input
        type="password"
        autoComplete="current-password"
        className="h-9 w-full rounded-md border bg-background px-2 font-mono text-sm outline-none focus:border-primary"
        placeholder="abk_…"
        value={key}
        onChange={(e) => setKey(e.target.value)}
        data-testid="router-signin-key"
        autoFocus
      />
      {error && <p className="text-xs text-destructive" data-testid="router-signin-error">{error}</p>}
      <Button type="submit" className="w-full" disabled={busy || !key.trim()} data-testid="router-signin-submit">
        {busy && <Loader2 className="animate-spin" />} Sign in
      </Button>
      {!s.hasKeys && (
        <p className="text-[11px] text-muted-foreground" data-testid="router-signin-bootstrap">
          No admin keys yet? On the router's host run <code className="rounded bg-muted px-1">abhoth router admin-key create &lt;name&gt; --control router.toml</code> (in Docker: <code className="rounded bg-muted px-1">docker exec … abhoth router admin-key …</code>).
        </p>
      )}
    </form>
  );
}

function Shell({ s, onSignedOut }: { s: RouterSession; onSignedOut: () => void }) {
  const [tab, setTab] = useState<TabId>(() => {
    const h = location.hash.slice(1) as TabId;
    return TABS.some((t) => t.id === h) ? h : "overview";
  });
  useEffect(() => {
    history.replaceState(null, "", `#${tab}`);
  }, [tab]);
  const tabs = TABS.filter((t) => t.id !== "keys" || s.localKeys);
  const signOut = async () => {
    await fetch(pageUrl("logout"), { method: "POST", credentials: "same-origin" }).catch(() => {});
    onSignedOut();
  };
  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="router-admin">
      <header className="flex items-center gap-3 border-b px-4 py-2">
        <Brand s={s} />
        <nav className="ml-4 flex min-w-0 flex-1 gap-1 overflow-x-auto">
          {tabs.map((t) => (
            <button
              key={t.id}
              onClick={() => setTab(t.id)}
              className={cn("flex shrink-0 items-center gap-1.5 rounded-md px-2.5 py-1.5 text-xs", tab === t.id ? "bg-accent text-foreground" : "text-muted-foreground hover:bg-accent/50")}
              data-testid={`router-tab-${t.id}`}
            >
              <t.icon className="size-3.5" /> {t.label}
            </button>
          ))}
        </nav>
        <span className="hidden text-[11px] text-muted-foreground sm:inline" data-testid="router-principal">{s.principal}</span>
        {s.via === "session" && (
          <Button size="sm" variant="ghost" onClick={() => void signOut()} data-testid="router-signout" aria-label="Sign out"><LogOut /></Button>
        )}
      </header>
      {!s.controlPlane && (
        <div className="border-b bg-muted/40 px-4 py-1.5 text-[11px] text-muted-foreground">
          The control plane is off (control.enabled = false): config, the model list and keys can be edited here; switching layouts is not available.
        </div>
      )}
      <main className="flex min-h-0 flex-1 flex-col">
        {tab === "overview" && <RouterOverviewPane />}
        {tab === "inventory" && <InventoryForms />}
        {tab === "store" && <EngineStore />}
        {tab === "setup" && <RouterSetupForms />}
        {tab === "models" && <GatewayForms />}
        {tab === "routes" && <RouterRoutesPane />}
        {tab === "advanced" && (
          <Suspense fallback={<Loader2 className="m-4 size-4 animate-spin" />}>
            <ConfigEditor />
          </Suspense>
        )}
        {tab === "activity" && <RouterActivityPane />}
        {tab === "keys" && <div className="min-h-0 flex-1 overflow-y-auto p-4"><AdminKeys /></div>}
      </main>
    </div>
  );
}
