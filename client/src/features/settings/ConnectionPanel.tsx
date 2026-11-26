import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, ExternalLink, KeyRound, Loader2, Network, Radar, Router as RouterIcon, Server, ShieldCheck } from "lucide-react";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { connectionApi, settingsApi, type DiscoveredRouter, type KeyRequestPollResult, type RouterInfo, type RouterModel, type Setting } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { CONNECTION_KEY, capabilityBadges, formatTokens, hiddenInMode, howToGetKey, keyHint, normalizeUrl, pickerOrder, routerAdminEntry, routerPick, savedNote, statusLine, waitLeft, type ConnectionMode, type KeyKind } from "./connection";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";

/**
 * The connection (D149): an Abhoth router, which supplies the models and their context window,
 * vision and tools and owns failover, slots and layouts; or an OpenAI-compatible server set up
 * here. Router mode is only ever offered (a URL that identifies as a router), never switched to
 * by itself. `pid`: the settings page's Project scope (that project's running session, changed as
 * /config in its terminal would); null: the server's config.json, which new sessions use.
 */
export function ConnectionPanel({ pid, mode, sections }: { pid: string | null; mode: ConnectionMode; sections: { name: string; settings: Setting[] }[] }) {
  const qc = useQueryClient();
  const status = useQuery({ queryKey: ["connection", pid ?? "server"], queryFn: () => connectionApi.status(pid), staleTime: 15000 });
  const [error, setError] = useState<string | null>(null);
  const [finding, setFinding] = useState(false);
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["settings"] });
    void qc.invalidateQueries({ queryKey: ["connection"] });
    void qc.invalidateQueries({ queryKey: ["router-models"] });
  };
  const adopt = useMutation({
    mutationFn: ({ url, model }: { url: string; model?: string }) => connectionApi.use(pid, url, model),
    onSuccess: () => {
      setError(null);
      setFinding(false);
      refresh();
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  const toServer = useMutation({
    mutationFn: () => settingsApi.set(pid, CONNECTION_KEY, "server"),
    onSuccess: (r) => {
      setError(r.ok ? null : (r.error ?? "Refused."));
      setFinding(false);
      refresh();
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  const line = statusLine(status.data ?? undefined);
  const hidden = hiddenInMode(sections, "router");
  const pick = (m: ConnectionMode) => {
    if (m === mode) return;
    setError(null);
    if (m === "server") return toServer.mutate();
    // Router mode on the configured URL when it is one, or on the one router found; otherwise find or type one.
    const found = qc.getQueryData<{ routers: DiscoveredRouter[] }>(["connection-discover"])?.routers;
    const next = routerPick(mode, status.data, found);
    if (next.kind === "adopt") adopt.mutate({ url: next.url });
    else if (next.kind === "find") setFinding(true);
  };
  const busy = adopt.isPending || toServer.isPending;
  const finder = finding || mode === "server";

  return (
    <section className="space-y-3 border-b py-3" data-testid="connection-panel" data-mode={mode}>
      <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Connection</h3>
      <div className="flex flex-wrap gap-2" role="radiogroup" aria-label="Connection type">
        <ModeButton on={mode === "router"} onClick={() => pick("router")} icon={<RouterIcon className="size-4" />} title="Abhoth router" sub="Models, context, vision and tools come from the router" testid="connection-router" />
        <ModeButton on={mode === "server"} onClick={() => pick("server")} icon={<Server className="size-4" />} title="OpenAI-compatible server" sub="LM Studio, llama.cpp, vLLM… set up here" testid="connection-server" />
      </div>
      {line.text && (
        <div className={cn("flex flex-wrap items-center gap-2 text-xs", line.offer ? "text-foreground" : "text-muted-foreground")} data-testid="connection-status">
          {mode === "router" && status.data?.router && <CheckCircle2 className="size-3.5 text-success" />}
          <span>{line.text}</span>
          {line.offer && status.data && (
            <Button size="sm" className="h-6" disabled={busy} onClick={() => adopt.mutate({ url: status.data!.baseUrl })} data-testid="connection-offer">
              Use router mode
            </Button>
          )}
        </div>
      )}
      {busy && (
        <div className="text-xs text-muted-foreground" data-testid="connection-busy">
          {adopt.isPending ? "Checking the router and switching…" : "Switching…"}
        </div>
      )}
      {error && !finder && <div className="text-xs text-destructive" data-testid="connection-error">{error}</div>}
      {finder && (
        <FindRouter
          busy={busy}
          prompt={finding}
          error={error}
          onUse={(url) => {
            setError(null);
            adopt.mutate({ url });
          }}
        />
      )}
      {mode === "router" && (
        <>
          {status.data?.router && <RouterSummary info={status.data.router} baseUrl={status.data.baseUrl} />}
          {status.data?.router && <KeyBox pid={pid} info={status.data.router} keyKind={status.data.apiKey} pending={status.data.keyRequest ?? null} onChanged={refresh} />}
          <RouterModels pid={pid} needsKey={!!status.data?.router?.auth.required && status.data?.apiKey !== "abhoth"} />
          {status.data?.router && <RouterAdmin />}
          {hidden.length > 0 && (
            <p className="text-xs text-muted-foreground" data-testid="connection-hidden">
              The router takes care of {hidden.length} settings here ({hidden.map((h) => h.label).join(", ")}), and of inference profiles, slots and GPU layouts.
              Switch to <i>OpenAI-compatible server</i> to set them yourself.
            </p>
          )}
        </>
      )}
      <p className="text-[11px] text-muted-foreground">{savedNote(!!pid)}</p>
    </section>
  );
}

function ModeButton({ on, onClick, icon, title, sub, testid }: { on: boolean; onClick: () => void; icon: React.ReactNode; title: string; sub: string; testid: string }) {
  return (
    <button
      role="radio"
      aria-checked={on}
      onClick={onClick}
      className={cn("flex min-w-[14rem] flex-1 items-start gap-2 rounded-md border p-2.5 text-left", on ? "border-primary bg-primary/10" : "hover:bg-accent/60")}
      data-testid={testid}
    >
      <span className={cn("mt-0.5", on ? "text-primary" : "text-muted-foreground")}>{icon}</span>
      <span className="min-w-0">
        <span className="block text-sm font-medium">{title}</span>
        <span className="block text-xs text-muted-foreground">{sub}</span>
      </span>
    </button>
  );
}

/** Routers on the LAN (mDNS) and a typed URL, each checked before it can be used. */
function FindRouter({ busy, prompt, error, onUse }: { busy: boolean; prompt: boolean; error: string | null; onUse: (url: string) => void }) {
  const [url, setUrl] = useState("");
  const input = useRef<HTMLInputElement>(null);
  // Asked for router mode with no router known: the URL box is where to go next.
  useEffect(() => {
    if (prompt) input.current?.focus();
  }, [prompt]);
  const found = useQuery({ queryKey: ["connection-discover"], queryFn: connectionApi.discover, staleTime: 60000, retry: false });
  const check = useMutation({ mutationFn: (u: string) => connectionApi.identify(normalizeUrl(u)) });
  const routers = found.data?.routers ?? [];
  return (
    <div className="space-y-2 rounded-md border border-dashed p-2.5" data-testid="connection-find">
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        <Radar className="size-3.5" />
        {found.isFetching ? "Looking for routers on the network…" : routers.length ? "Found on the network:" : (found.data?.error ?? "No router answered on the network.")}
        <Button size="sm" variant="ghost" className="ml-auto h-6" disabled={found.isFetching} onClick={() => void found.refetch()} data-testid="connection-rescan">
          Search again
        </Button>
      </div>
      {prompt && (
        <div className="text-xs text-foreground" data-testid="connection-prompt">
          Pick a router found on the network, or type its URL and Check, then Use it.
        </div>
      )}
      {routers.map((r) => (
        <Found key={r.url} r={r} busy={busy} onUse={onUse} />
      ))}
      <form
        className="flex gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          if (url.trim()) check.mutate(url);
        }}
      >
        <input ref={input} className={field} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="Router URL, e.g. http://gpu-host:8010" aria-label="Router URL" data-testid="connection-url" />
        <Button type="submit" size="sm" className="h-8" variant="outline" disabled={!url.trim() || check.isPending} data-testid="connection-check">
          Check
        </Button>
      </form>
      {check.data && (
        <div className="text-xs" data-testid="connection-check-result">
          {check.data.router ? (
            <span className="flex flex-wrap items-center gap-2">
              <CheckCircle2 className="size-3.5 text-success" />
              {check.data.router.name} (v{check.data.router.version}) at {check.data.url}
              <Button size="sm" className="h-6" disabled={busy} onClick={() => onUse(check.data!.url)} data-testid="connection-check-use">
                Use it
              </Button>
            </span>
          ) : (
            <span className="text-muted-foreground">{check.data.error ? `No answer: ${check.data.error}.` : "That is not an Abhoth router (an OpenAI-compatible server is set up with the other option)."}</span>
          )}
        </div>
      )}
      {check.isError && <div className="text-xs text-destructive">{(check.error as Error).message}</div>}
      {error && (
        <div className="text-xs text-destructive" data-testid="connection-error">
          Could not switch to the router: {error}
        </div>
      )}
    </div>
  );
}

function Found({ r, busy, onUse }: { r: DiscoveredRouter; busy: boolean; onUse: (url: string) => void }) {
  return (
    <div className="flex flex-wrap items-center gap-2 text-sm" data-testid="connection-found">
      <Network className="size-3.5 text-muted-foreground" />
      <span className="font-medium">{r.name}</span>
      <span className="font-mono text-xs text-muted-foreground">{r.url}</span>
      {r.version && <span className="text-xs text-muted-foreground">v{r.version}</span>}
      {r.info?.auth.required && (
        <span className="inline-flex items-center gap-0.5 text-xs text-muted-foreground">
          <KeyRound className="size-3" /> key needed
        </span>
      )}
      {!r.verified && <span className="text-xs text-warning">did not answer</span>}
      <Button size="sm" className="ml-auto h-6" disabled={busy || !r.verified} onClick={() => onUse(r.url)} data-testid="connection-use">
        Use it
      </Button>
    </div>
  );
}

/**
 * "Router admin" (D151): when the app's router key is an admin's, the router's own admin pages
 * (servers, cards, layouts, the model list, keys), inside the app through this server (the key
 * never reaches the page), or in the browser at the router (sign in there with an admin key).
 */
function RouterAdmin() {
  const doc = useQuery({ queryKey: ["connection-router-admin"], queryFn: connectionApi.routerAdmin, staleTime: 30000, retry: false });
  const [open, setOpen] = useState(false);
  const entry = routerAdminEntry("router", doc.data);
  if (!entry.show) return null;
  return (
    <div className="space-y-1.5 rounded-md border p-3" data-testid="connection-router-admin">
      <div className="flex items-center gap-2 text-sm font-medium"><ShieldCheck className="size-4 text-success" /> Router admin</div>
      <p className="text-xs text-muted-foreground">
        This app's key is an admin key on the router ({doc.data?.principal}). Its servers, cards, engines, layouts and model list are edited on the router's own admin pages.
      </p>
      <div className="flex flex-wrap gap-2">
        <Button size="sm" onClick={() => setOpen(true)} data-testid="connection-router-admin-open">Open router admin</Button>
        {entry.url && (
          <Button size="sm" variant="ghost" asChild>
            <a href={entry.url} target="_blank" rel="noreferrer noopener" data-testid="connection-router-admin-browser"><ExternalLink /> Open in browser</a>
          </Button>
        )}
      </div>
      {open && entry.embedded && (
        <Dialog open onOpenChange={(o) => !o && setOpen(false)}>
          <DialogContent className="flex h-[90vh] max-w-[min(1400px,95vw)] flex-col gap-2 p-3" data-testid="router-admin-dialog">
            <DialogTitle className="text-sm">Router admin</DialogTitle>
            <DialogDescription className="sr-only">The router's own admin pages, through this server.</DialogDescription>
            <iframe src={entry.embedded} title="Router admin" className="min-h-0 w-full flex-1 rounded border bg-background" data-testid="router-admin-frame" />
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
}

function RouterSummary({ info, baseUrl }: { info: RouterInfo; baseUrl: string }) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground" data-testid="connection-router-info">
      <span>
        <b className="text-foreground">{info.name}</b> v{info.version}
      </span>
      <span className="font-mono">{baseUrl}</span>
    </div>
  );
}

/**
 * The router's key, in one place: what the configured key gives, a field to paste one (masked,
 * never shown back), Remove, and "Request a key" (an admin approves it; this server polls the
 * router and saves the key, which never comes to the page).
 */
function KeyBox({ pid, info, keyKind, pending, onChanged }: { pid: string | null; info: RouterInfo; keyKind?: KeyKind; pending: KeyRequestPollResult["request"]; onChanged: () => void }) {
  const models = useQuery({ queryKey: ["router-models", pid ?? "server"], queryFn: () => connectionApi.models(pid), staleTime: 15000, retry: false });
  const hasClaude = info.capabilities.includes("claude");
  const hint = keyHint(keyKind, info.auth, hasClaude, models.data ? models.data.claude : undefined);
  const [key, setKey] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [waiting, setWaiting] = useState<KeyRequestPollResult["request"]>(pending);
  useEffect(() => setWaiting(pending), [pending?.code]); // eslint-disable-line react-hooks/exhaustive-deps
  const fail = (e: unknown) => setError(e instanceof Error ? e.message : String(e));
  const save = useMutation({
    mutationFn: () => connectionApi.saveKey(pid, key),
    onSuccess: () => {
      setKey("");
      setError(null);
      setNote("Key saved. The models below are what it may use.");
      onChanged();
    },
    onError: fail,
  });
  const remove = useMutation({
    mutationFn: () => connectionApi.removeKey(pid),
    onSuccess: () => {
      setError(null);
      setNote("Key removed.");
      onChanged();
    },
    onError: fail,
  });
  const ask = useMutation({
    mutationFn: () => connectionApi.requestKey(pid),
    onSuccess: (r) => {
      setError(null);
      setNote(null);
      setWaiting(r.request ?? null);
    },
    onError: fail,
  });
  const cancel = useMutation({ mutationFn: () => connectionApi.forgetKeyRequest(pid), onSuccess: () => setWaiting(null) });
  // While a request waits: ask this server every 3 s (it asks the router; an approved key is saved there).
  const poll = useQuery({
    queryKey: ["key-request", pid ?? "server", waiting?.code],
    queryFn: () => connectionApi.pollKey(pid),
    enabled: !!waiting,
    refetchInterval: 3000,
    retry: false,
  });
  useEffect(() => {
    const r = poll.data;
    if (!r || !waiting) return;
    if (r.status === "pending") {
      if (r.request) setWaiting(r.request);
      return;
    }
    setWaiting(null);
    if (r.status === "approved") {
      setNote(`An admin approved the request: key "${r.name ?? ""}" is saved${r.claude ? " (it may use Claude)" : ""}.`);
      onChanged();
    } else if (r.status === "denied") setError("An admin denied the key request.");
    else if (r.status === "expired") setError("The key request expired before an admin approved it. Request a key again.");
  }, [poll.data]); // eslint-disable-line react-hooks/exhaustive-deps
  const busy = save.isPending || remove.isPending || ask.isPending;
  const canAsk = !!info.auth.keyRequests && keyKind !== "abhoth" && !waiting;
  return (
    <div className="space-y-2 rounded-md border p-2.5" data-testid="connection-key">
      <div className="flex items-center gap-2 text-sm font-medium">
        <KeyRound className="size-4 text-muted-foreground" /> Abhoth key
        {keyKind === "abhoth" && <span className="rounded bg-success/15 px-1.5 text-[11px] font-normal text-success">saved</span>}
      </div>
      <div className={cn("text-xs", hint.tone === "warn" ? "text-warning" : "text-muted-foreground")} data-testid="connection-key-hint">
        {hint.text}
      </div>
      {info.auth.keys && (
        <form
          className="flex flex-wrap gap-1"
          onSubmit={(e) => {
            e.preventDefault();
            if (key.trim()) save.mutate();
          }}
        >
          <input
            type="password"
            autoComplete="off"
            spellCheck={false}
            className={cn(field, "min-w-[14rem] flex-1 font-mono")}
            value={key}
            onChange={(e) => setKey(e.target.value)}
            placeholder={keyKind === "abhoth" ? "Paste a new key to replace it" : `Paste an Abhoth key (${info.auth.keyPrefix ?? "abk_"}…)`}
            aria-label="Abhoth key"
            data-testid="connection-key-input"
          />
          <Button type="submit" size="sm" className="h-8" disabled={!key.trim() || busy} data-testid="connection-key-save">
            Save
          </Button>
          {keyKind === "abhoth" && (
            <Button type="button" size="sm" variant="outline" className="h-8" disabled={busy} onClick={() => remove.mutate()} data-testid="connection-key-remove">
              Remove
            </Button>
          )}
          {canAsk && (
            <Button type="button" size="sm" variant="outline" className="h-8" disabled={busy} onClick={() => ask.mutate()} data-testid="connection-key-request">
              {ask.isPending ? <Loader2 className="animate-spin" /> : null} Request a key
            </Button>
          )}
        </form>
      )}
      {waiting && (
        <div className="space-y-1 rounded-md border border-primary/40 bg-primary/5 p-2 text-xs" role="status" data-testid="connection-key-pending">
          <div className="flex flex-wrap items-center gap-2">
            <Loader2 className="size-3.5 animate-spin text-primary" />
            <span>
              Waiting for an admin to approve code{" "}
              <b className="font-mono text-sm tracking-wider text-foreground" data-testid="connection-key-code">
                {waiting.code}
              </b>{" "}
              ({waitLeft(waiting.expiresIn)} left).
            </span>
          </div>
          <div className="text-muted-foreground">
            An admin approves it in the router's web UI, Settings &gt; Service keys (signed in there as an admin, you can approve your own request in one click). The key is saved here by itself once it is approved.
          </div>
          <div className="flex flex-wrap gap-3">
            {waiting.keyUrl && (
              <a className="inline-flex items-center gap-1 text-primary hover:underline" href={waiting.keyUrl} target="_blank" rel="noreferrer" data-testid="connection-key-approve-link">
                Open the approval page <ExternalLink className="size-3" />
              </a>
            )}
            <button type="button" className="text-muted-foreground hover:underline" onClick={() => cancel.mutate()}>
              Cancel
            </button>
          </div>
        </div>
      )}
      {note && !error && (
        <div className="text-xs text-success" data-testid="connection-key-note">
          {note}
        </div>
      )}
      {error && (
        <div className="text-xs text-destructive" data-testid="connection-key-error">
          {error}
        </div>
      )}
      {keyKind !== "abhoth" && !waiting && (
        <p className="text-[11px] text-muted-foreground" data-testid="connection-key-how">
          {howToGetKey(info.auth)}
        </p>
      )}
    </div>
  );
}

/** The router's models as this config's key sees them; the default model is picked here. */
function RouterModels({ pid, needsKey }: { pid: string | null; needsKey: boolean }) {
  const qc = useQueryClient();
  const models = useQuery({ queryKey: ["router-models", pid ?? "server"], queryFn: () => connectionApi.models(pid), staleTime: 15000, retry: false });
  const [error, setError] = useState<string | null>(null);
  const choose = useMutation({
    mutationFn: (id: string) => settingsApi.set(pid, "lmStudio.models", id ? [id] : []),
    onSuccess: (r) => {
      setError(r.ok ? null : (r.error ?? "Refused."));
      void qc.invalidateQueries({ queryKey: ["router-models"] });
      void qc.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  if (models.isLoading) return <div className="text-xs text-muted-foreground">Asking the router for its models…</div>;
  if (models.isError)
    return needsKey ? (
      <div className="text-xs text-muted-foreground" data-testid="router-models-error">The router shows its models once a key is saved (above).</div>
    ) : (
      <div className="text-xs text-destructive" data-testid="router-models-error">The router's models: {(models.error as Error).message}</div>
    );
  const doc = models.data;
  if (!doc) return null;
  const ordered = pickerOrder(doc.models);
  const chosen = doc.chosen ?? "";
  const known = ordered.some((m) => m.id === chosen);
  return (
    <div className="space-y-2" data-testid="router-models">
      <label className="flex flex-wrap items-center gap-2 text-sm">
        <span className="font-medium">Default model</span>
        <select
          className="h-8 min-w-[14rem] rounded-md border bg-background px-2 text-sm"
          value={chosen}
          disabled={choose.isPending}
          onChange={(e) => choose.mutate(e.target.value)}
          data-testid="router-default-model"
        >
          <option value="">(the router's first live model)</option>
          {chosen && !known && <option value={chosen}>{chosen} (not offered to this key)</option>}
          {ordered.map((m) => (
            <option key={m.id} value={m.id}>
              {m.id}
              {m.status === "live" ? " • live" : m.status ? ` • ${m.status}` : ""}
            </option>
          ))}
        </select>
        <span className="text-xs text-muted-foreground">
          Claude: <b className={doc.claude ? "text-success" : "text-foreground"}>{doc.claude ? "available to this key" : "not available to this key"}</b>
        </span>
      </label>
      {error && <div className="text-xs text-destructive">{error}</div>}
      <div className="max-h-56 overflow-y-auto rounded-md border">
        <table className="w-full text-xs" data-testid="router-models-table">
          <thead className="sticky top-0 bg-panel text-left text-muted-foreground">
            <tr>
              <th className="px-2 py-1 font-medium">Model</th>
              <th className="px-2 py-1 font-medium">Context</th>
              <th className="px-2 py-1 font-medium">Can</th>
              <th className="px-2 py-1 font-medium">State</th>
            </tr>
          </thead>
          <tbody className="divide-y">
            {ordered.map((m) => (
              <ModelRow key={m.id} m={m} chosen={m.id === chosen} />
            ))}
          </tbody>
        </table>
      </div>
      {!!doc.cards?.length && (
        <div className="flex flex-wrap gap-1.5 text-[11px] text-muted-foreground" data-testid="router-cards">
          Cards:
          {doc.cards.map((c) => (
            <span key={c.id} className={cn("rounded border px-1.5", !c.online && "opacity-50")}>
              {c.id} {c.engine ?? ""} {c.model ? `· ${c.model}` : ""}
            </span>
          ))}
        </div>
      )}
      <p className="text-[11px] text-muted-foreground">
        Shown as the router sees this key ({doc.principal}). Context window, vision and tool calling follow the model; the router fails over between cards and handles cold models.
      </p>
    </div>
  );
}

function ModelRow({ m, chosen }: { m: RouterModel; chosen: boolean }) {
  return (
    <tr className={cn(chosen && "bg-primary/10")} data-testid="router-model">
      <td className="px-2 py-1 font-mono">{m.id}</td>
      <td className="px-2 py-1 font-mono">{formatTokens(m.contextWindow)}</td>
      <td className="px-2 py-1">
        <span className="flex flex-wrap gap-1">
          {capabilityBadges(m).map((b) => (
            <span key={b} className="rounded bg-muted px-1">
              {b}
            </span>
          ))}
        </span>
      </td>
      <td className={cn("px-2 py-1", m.status === "live" ? "text-success" : "text-muted-foreground")}>{m.status ?? (m.kind === "cloud" ? "cloud" : "")}</td>
    </tr>
  );
}
