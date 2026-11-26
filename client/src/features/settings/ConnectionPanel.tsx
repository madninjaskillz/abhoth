import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, KeyRound, Network, Radar, Router as RouterIcon, Server } from "lucide-react";
import { connectionApi, settingsApi, type DiscoveredRouter, type RouterInfo, type RouterModel, type Setting } from "@/lib/api";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { CONNECTION_KEY, capabilityBadges, formatTokens, hiddenInMode, normalizeUrl, pickerOrder, statusLine, type ConnectionMode } from "./connection";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";

/**
 * The connection (D149): an Abhoth router, which supplies the models and their context window,
 * vision and tools and owns failover, slots and layouts; or an OpenAI-compatible server set up
 * here. Router mode is only ever offered (a URL that identifies as a router), never switched to
 * by itself. Saved to the server's config.json; new sessions use it.
 */
export function ConnectionPanel({ mode, sections }: { mode: ConnectionMode; sections: { name: string; settings: Setting[] }[] }) {
  const qc = useQueryClient();
  const status = useQuery({ queryKey: ["connection"], queryFn: connectionApi.status, staleTime: 15000 });
  const [error, setError] = useState<string | null>(null);
  const [finding, setFinding] = useState(false);
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ["settings"] });
    void qc.invalidateQueries({ queryKey: ["connection"] });
    void qc.invalidateQueries({ queryKey: ["router-models"] });
  };
  const adopt = useMutation({
    mutationFn: ({ url, model }: { url: string; model?: string }) => connectionApi.use(url, model),
    onSuccess: () => {
      setError(null);
      setFinding(false);
      refresh();
    },
    onError: (e) => setError(e instanceof Error ? e.message : String(e)),
  });
  const toServer = useMutation({
    mutationFn: () => settingsApi.set(null, CONNECTION_KEY, "server"),
    onSuccess: (r) => {
      setError(r.ok ? null : (r.error ?? "Refused."));
      refresh();
    },
  });
  const line = statusLine(status.data ?? undefined);
  const hidden = hiddenInMode(sections, "router");
  const pick = (m: ConnectionMode) => {
    if (m === mode) return;
    if (m === "server") return toServer.mutate();
    // Router mode on the configured URL when it is one; otherwise find or type one.
    if (status.data?.router) adopt.mutate({ url: status.data.baseUrl });
    else setFinding(true);
  };

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
            <Button size="sm" className="h-6" disabled={adopt.isPending} onClick={() => adopt.mutate({ url: status.data!.baseUrl })} data-testid="connection-offer">
              Use router mode
            </Button>
          )}
        </div>
      )}
      {error && <div className="text-xs text-destructive" data-testid="connection-error">{error}</div>}
      {(finding || mode === "server") && <FindRouter busy={adopt.isPending} onUse={(url) => adopt.mutate({ url })} />}
      {mode === "router" && (
        <>
          {status.data?.router && <RouterSummary info={status.data.router} baseUrl={status.data.baseUrl} />}
          <RouterModels />
          {hidden.length > 0 && (
            <p className="text-xs text-muted-foreground" data-testid="connection-hidden">
              The router takes care of {hidden.length} settings here ({hidden.map((h) => h.label).join(", ")}), and of inference profiles, slots and GPU layouts.
              Switch to <i>OpenAI-compatible server</i> to set them yourself.
            </p>
          )}
        </>
      )}
      <p className="text-[11px] text-muted-foreground">Saved to the server's config.json: new sessions use it (restart the app to update the profile picker).</p>
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
function FindRouter({ busy, onUse }: { busy: boolean; onUse: (url: string) => void }) {
  const [url, setUrl] = useState("");
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
        <input className={field} value={url} onChange={(e) => setUrl(e.target.value)} placeholder="Router URL, e.g. http://gpu-host:8010" aria-label="Router URL" data-testid="connection-url" />
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
              <Button size="sm" className="h-6" disabled={busy} onClick={() => onUse(check.data!.url)}>
                Use it
              </Button>
            </span>
          ) : (
            <span className="text-muted-foreground">{check.data.error ? `No answer: ${check.data.error}.` : "That is not an Abhoth router (an OpenAI-compatible server is set up with the other option)."}</span>
          )}
        </div>
      )}
      {check.isError && <div className="text-xs text-destructive">{(check.error as Error).message}</div>}
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

function RouterSummary({ info, baseUrl }: { info: RouterInfo; baseUrl: string }) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground" data-testid="connection-router-info">
      <span>
        <b className="text-foreground">{info.name}</b> v{info.version}
      </span>
      <span className="font-mono">{baseUrl}</span>
      {info.auth.required ? (
        <span className="inline-flex items-center gap-1">
          <KeyRound className="size-3" /> needs an API key ({info.auth.keyPrefix ?? "abk_"}…) in API Key below
          {info.auth.keyUrl && (
            <a className="text-primary hover:underline" href={info.auth.keyUrl} target="_blank" rel="noreferrer">
              make one
            </a>
          )}
        </span>
      ) : (
        <span>no key needed from here</span>
      )}
    </div>
  );
}

/** The router's models as this config's key sees them; the default model is picked here. */
function RouterModels() {
  const qc = useQueryClient();
  const models = useQuery({ queryKey: ["router-models"], queryFn: connectionApi.models, staleTime: 15000, retry: false });
  const [error, setError] = useState<string | null>(null);
  const choose = useMutation({
    mutationFn: (id: string) => settingsApi.set(null, "lmStudio.models", id ? [id] : []),
    onSuccess: (r) => {
      setError(r.ok ? null : (r.error ?? "Refused."));
      void qc.invalidateQueries({ queryKey: ["router-models"] });
      void qc.invalidateQueries({ queryKey: ["settings"] });
    },
  });
  if (models.isLoading) return <div className="text-xs text-muted-foreground">Asking the router for its models…</div>;
  if (models.isError) return <div className="text-xs text-destructive" data-testid="router-models-error">The router's models: {(models.error as Error).message}</div>;
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
