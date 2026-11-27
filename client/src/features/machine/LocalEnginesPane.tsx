// Settings → Local Engines (LOCAL-ENGINES.md, D156): every engine this machine uses, managed
// (This Machine's inventory) or not (a kind, a name and a URL), and the Cloud providers.
import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Cloud, Cpu, Pencil, Plus, Server, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Badge } from "@/components/ui/misc";
import { openSettings } from "@/features/settings/settingsNav";
import {
  API_LABELS,
  CLOUD_PRESETS,
  KIND_LABELS,
  idFrom,
  isLocalUrl,
  localEnginesApi,
  parseModels,
  toolChoices,
  type CheckResult,
  type Endpoint,
  type EndpointGroup,
  type EndpointView,
  type EngineKind,
  type LocalEngines,
  type ManagedEngine,
} from "./localEngines";

const field = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";
const QK = ["local-engines"];

export function LocalEnginesPane() {
  const q = useQuery({ queryKey: QK, queryFn: localEnginesApi.get, refetchInterval: 5000 });
  const [editing, setEditing] = useState<{ id: string | null; group: EndpointGroup } | null>(null);
  if (q.isLoading) return <div className="p-4 text-sm text-muted-foreground">Loading…</div>;
  if (q.isError || !q.data) return <div className="p-4 text-sm text-destructive">Local Engines could not be read: {String((q.error as Error)?.message ?? "no answer")}</div>;
  const le = q.data;
  if (le.error) return <div className="p-4 text-sm text-destructive">{le.error}</div>;
  const unmanaged = le.endpoints.filter((e) => e.group !== "cloud");
  const cloud = le.endpoints.filter((e) => e.group === "cloud");
  return (
    <div className="space-y-5 p-4 text-sm" data-testid="local-engines">
      <p className="text-xs text-muted-foreground">
        Every engine this machine uses. <b>Managed</b> engines are installed and run by Abhoth (This Machine). <b>Unmanaged</b> engines already run, here or elsewhere: a kind, a name and a URL is
        enough. <b>Cloud</b> providers take a key.
      </p>
      {le.clashes.length > 0 && (
        <div className="rounded-md border border-warning/40 bg-warning/10 p-2 text-xs" data-testid="local-engines-clashes">
          {le.clashes.map((c) => (
            <div key={c}>{c}</div>
          ))}
        </div>
      )}
      <Section icon={<Cpu />} title="On this PC (managed)" blurb="Run by Abhoth on this machine's cards. Install, start and set them up on This Machine." testid="le-managed">
        {le.managed.length === 0 && <Empty>No managed engines yet. Add one from the engine store on This Machine.</Empty>}
        {le.managed.map((m) => (
          <ManagedRow key={m.id} m={m} le={le} />
        ))}
        <div>
          <Button size="sm" variant="outline" onClick={() => openSettings("machine")} data-testid="le-open-machine">
            Open This Machine
          </Button>
        </div>
      </Section>
      <Section icon={<Server />} title="Unmanaged" blurb="Engines that already run (LM Studio, Ollama, a ComfyUI on the NAS…). Abhoth only sends requests to them." testid="le-unmanaged">
        {unmanaged.length === 0 && <Empty>None yet.</Empty>}
        {unmanaged.map((e) => (
          <EndpointRow key={e.id} e={e} le={le} onEdit={() => setEditing({ id: e.id, group: "unmanaged" })} />
        ))}
        {editing?.group === "unmanaged" && <EndpointForm le={le} id={editing.id} group="unmanaged" onDone={() => setEditing(null)} />}
        {editing?.group !== "unmanaged" && (
          <div>
            <Button size="sm" variant="outline" onClick={() => setEditing({ id: null, group: "unmanaged" })} data-testid="le-add-unmanaged">
              <Plus /> Add an unmanaged engine
            </Button>
          </div>
        )}
      </Section>
      <Section icon={<Cloud />} title="Cloud" blurb="Hosted providers (Anthropic, OpenAI, OpenRouter…). Keys are stored on this machine and never shown again." testid="le-cloud">
        {cloud.length === 0 && <Empty>None yet.</Empty>}
        {cloud.map((e) => (
          <EndpointRow key={e.id} e={e} le={le} onEdit={() => setEditing({ id: e.id, group: "cloud" })} />
        ))}
        {editing?.group === "cloud" && <EndpointForm le={le} id={editing.id} group="cloud" onDone={() => setEditing(null)} />}
        {editing?.group !== "cloud" && (
          <div>
            <Button size="sm" variant="outline" onClick={() => setEditing({ id: null, group: "cloud" })} data-testid="le-add-cloud">
              <Plus /> Add a Cloud provider
            </Button>
          </div>
        )}
      </Section>
      <ToolDefaults le={le} />
    </div>
  );
}

function Section({ icon, title, blurb, testid, children }: { icon: React.ReactNode; title: string; blurb: string; testid: string; children: React.ReactNode }) {
  return (
    <section className="space-y-2" data-testid={testid}>
      <div>
        <h3 className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground [&_svg]:size-3.5">
          {icon}
          {title}
        </h3>
        <p className="text-xs text-muted-foreground">{blurb}</p>
      </div>
      {children}
    </section>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="rounded-md border border-dashed p-3 text-xs text-muted-foreground">{children}</div>;
}

function useToggle() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (f: () => Promise<unknown>) => f(),
    onSettled: () => qc.invalidateQueries({ queryKey: QK }),
  });
}

function Toggle({ label, checked, disabled, title, onChange, testid }: { label: string; checked: boolean; disabled?: boolean; title?: string; onChange: (v: boolean) => void; testid: string }) {
  return (
    <label className="flex items-center gap-1.5 text-xs" title={title}>
      <input type="checkbox" checked={checked} disabled={disabled} onChange={(e) => onChange(e.target.checked)} data-testid={testid} />
      <span className={disabled ? "text-muted-foreground" : ""}>{label}</span>
    </label>
  );
}

function stateBadge(states: ManagedEngine["states"]) {
  if (states.some((s) => s.state === "running")) return <Badge variant="success">running</Badge>;
  if (states.some((s) => s.state === "starting")) return <Badge variant="warning">starting</Badge>;
  if (states.some((s) => s.state === "failed")) return <Badge variant="destructive">failed</Badge>;
  return <Badge variant="outline">stopped</Badge>;
}

function ManagedRow({ m, le }: { m: ManagedEngine; le: LocalEngines }) {
  const t = useToggle();
  const toolKind = m.kinds.find((k) => le.tools[k]);
  return (
    <div className="rounded-md border p-2.5" data-testid="le-row" data-id={m.id}>
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">{m.title}</span>
        {m.kinds.map((k) => (
          <Badge key={k} variant="secondary">
            {KIND_LABELS[k] ?? k}
          </Badge>
        ))}
        {stateBadge(m.states)}
        <span className="text-xs text-muted-foreground">{m.cards.join(", ")}</span>
      </div>
      <div className="mt-1 text-xs text-muted-foreground">{m.models.length ? m.models.join(", ") : "No models yet"}</div>
      <div className="mt-2 flex flex-wrap gap-4">
        <Toggle
          label="Expose on Migo Bridge"
          checked={m.shared}
          title={le.bridgeMode ? undefined : "Used while Migo Bridge is on"}
          onChange={(v) => t.mutate(() => localEnginesApi.share({ engines: { [m.id]: v } }))}
          testid="le-share"
        />
        {toolKind && (
          <Toggle label={`Expose as tool (${le.tools[toolKind]})`} checked={m.tool} onChange={(v) => t.mutate(() => localEnginesApi.tools({ exposed: { [m.id]: v } }))} testid="le-tool" />
        )}
      </div>
    </div>
  );
}

function EndpointRow({ e, le, onEdit }: { e: EndpointView; le: LocalEngines; onEdit: () => void }) {
  const t = useToggle();
  const qc = useQueryClient();
  const [confirm, setConfirm] = useState(false);
  const remove = useMutation({ mutationFn: () => localEnginesApi.remove(e.id), onSettled: () => qc.invalidateQueries({ queryKey: QK }) });
  const tool = le.tools[e.kind];
  const relayed = !isLocalUrl(e.url);
  return (
    <div className="rounded-md border p-2.5" data-testid="le-row" data-id={e.id}>
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">{e.title}</span>
        <Badge variant="secondary">{KIND_LABELS[e.kind] ?? e.kind}</Badge>
        <span className="text-xs text-muted-foreground">{API_LABELS[e.api] ?? e.api}</span>
        {e.keySet && <Badge variant="outline">key set</Badge>}
        <span className="ml-auto flex gap-1">
          <Button size="icon-sm" variant="ghost" aria-label={`Edit ${e.title}`} onClick={onEdit} data-testid="le-edit">
            <Pencil />
          </Button>
          <Button size="icon-sm" variant="ghost" aria-label={`Remove ${e.title}`} onClick={() => setConfirm(true)} data-testid="le-remove">
            <Trash2 />
          </Button>
        </span>
      </div>
      <div className="mt-1 break-all font-mono text-xs text-muted-foreground">{e.url}</div>
      <div className="mt-1 text-xs text-muted-foreground">{e.models.length ? e.models.join(", ") : "Engine default"}</div>
      <div className="mt-2 flex flex-wrap gap-4">
        {e.group !== "cloud" && (
          <Toggle
            label={relayed ? "Expose on Migo Bridge (relayed through this PC)" : "Expose on Migo Bridge"}
            checked={e.shared}
            disabled
            title="Migo Bridge offers unmanaged engines from a later release; the choice is kept until then."
            onChange={(v) => t.mutate(() => localEnginesApi.share({ endpoints: { [e.id]: v } }))}
            testid="le-share"
          />
        )}
        {tool && <Toggle label={`Expose as tool (${tool})`} checked={e.tool} onChange={(v) => t.mutate(() => localEnginesApi.tools({ exposed: { [e.id]: v } }))} testid="le-tool" />}
      </div>
      <ConfirmDialog
        open={confirm}
        title={`Remove ${e.title}?`}
        body="Its key (if any) is deleted from this machine too."
        confirm="Remove"
        destructive
        onConfirm={() => {
          setConfirm(false);
          remove.mutate();
        }}
        onCancel={() => setConfirm(false)}
      />
    </div>
  );
}

function EndpointForm({ le, id, group, onDone }: { le: LocalEngines; id: string | null; group: EndpointGroup; onDone: () => void }) {
  const qc = useQueryClient();
  const old = id ? le.endpoints.find((e) => e.id === id) : undefined;
  const [title, setTitle] = useState(old?.title ?? (group === "cloud" ? CLOUD_PRESETS[0].title : ""));
  const [kind, setKind] = useState<EngineKind>(old?.kind ?? "llm");
  const [url, setUrl] = useState(old?.url ?? (group === "cloud" ? CLOUD_PRESETS[0].url : ""));
  const [apiStyle, setApi] = useState(old?.api ?? (group === "cloud" ? CLOUD_PRESETS[0].api : "openai"));
  const [models, setModels] = useState((old?.models ?? []).join("\n"));
  const [engineDefault, setEngineDefault] = useState(old ? old.models.length === 0 : group !== "cloud");
  const [key, setKey] = useState("");
  const [clearKey, setClearKey] = useState(false);
  const [timeout, setTimeoutS] = useState(old?.timeoutSeconds ? String(old.timeoutSeconds) : "");
  const [uncensored, setUncensored] = useState(old?.uncensoredModel ?? "");
  const [checked, setChecked] = useState<CheckResult | null>(null);
  const [error, setError] = useState("");
  const taken = useMemo(() => new Set([...le.endpoints.map((e) => e.id), ...le.managed.map((m) => m.id)]), [le]);
  const apis = le.apis[kind] ?? [];
  const endpoint = (): Endpoint => ({
    title: title.trim(),
    group,
    kind,
    url: url.trim(),
    api: apis.includes(apiStyle) ? apiStyle : apis[0] ?? "",
    models: engineDefault ? [] : parseModels(models),
    timeoutSeconds: timeout.trim() ? Number(timeout) : null,
    uncensoredModel: kind === "image" && uncensored.trim() ? uncensored.trim() : null,
  });
  const keyArg = () => (clearKey ? "" : key.trim() ? key.trim() : undefined);
  const save = useMutation({
    mutationFn: () => localEnginesApi.save(id ?? idFrom(title, taken), endpoint(), le.rev, keyArg()),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: QK });
      onDone();
    },
    onError: (e) => setError((e as Error).message),
  });
  const check = useMutation({
    mutationFn: () => localEnginesApi.check(endpoint(), id ?? undefined, key.trim() || undefined),
    onSuccess: (r) => setChecked(r),
    onError: (e) => setChecked({ ok: false, error: (e as Error).message }),
  });
  const preset = (p: (typeof CLOUD_PRESETS)[number]) => {
    setTitle(p.title);
    setUrl(p.url);
    setApi(p.api);
  };
  return (
    <form
      className="space-y-2 rounded-md border border-primary/40 bg-accent/30 p-3"
      onSubmit={(ev) => {
        ev.preventDefault();
        setError("");
        save.mutate();
      }}
      data-testid="le-form"
    >
      <div className="text-xs font-medium">{old ? `Edit ${old.title}` : group === "cloud" ? "Add a Cloud provider" : "Add an unmanaged engine"}</div>
      {group === "cloud" && !old && (
        <div className="flex flex-wrap gap-1">
          {CLOUD_PRESETS.map((p) => (
            <Button key={p.title} type="button" size="sm" variant={title === p.title ? "secondary" : "ghost"} onClick={() => preset(p)}>
              {p.title}
            </Button>
          ))}
        </div>
      )}
      <div className="grid gap-2 sm:grid-cols-2">
        <label className="space-y-1 text-xs">
          <span>Name</span>
          <input className={field} value={title} onChange={(e) => setTitle(e.target.value)} placeholder={group === "cloud" ? "Anthropic" : "LM Studio"} data-testid="le-title" />
        </label>
        <label className="space-y-1 text-xs">
          <span>Kind</span>
          <select
            className={field}
            value={kind}
            onChange={(e) => {
              const k = e.target.value as EngineKind;
              setKind(k);
              setApi(le.apis[k]?.[0] ?? "");
            }}
            data-testid="le-kind"
          >
            {le.kinds.map((k) => (
              <option key={k} value={k}>
                {KIND_LABELS[k] ?? k}
              </option>
            ))}
          </select>
        </label>
        <label className="space-y-1 text-xs sm:col-span-2">
          <span>URL</span>
          <input className={field} value={url} onChange={(e) => setUrl(e.target.value)} placeholder={kind === "llm" ? "http://127.0.0.1:1234/v1" : "http://127.0.0.1:8188"} data-testid="le-url" />
        </label>
        <label className="space-y-1 text-xs">
          <span>API</span>
          <select className={field} value={apis.includes(apiStyle) ? apiStyle : apis[0]} onChange={(e) => setApi(e.target.value)} data-testid="le-api">
            {apis.map((a) => (
              <option key={a} value={a}>
                {API_LABELS[a] ?? a}
              </option>
            ))}
          </select>
        </label>
        <label className="space-y-1 text-xs">
          <span>Key {old?.keySet ? "(set; type a new one to replace it)" : group === "cloud" ? "" : "(optional)"}</span>
          <input
            className={field}
            type="password"
            autoComplete="off"
            value={key}
            disabled={clearKey}
            onChange={(e) => setKey(e.target.value)}
            placeholder={old?.keySet ? "••••••••" : ""}
            data-testid="le-key"
          />
          {old?.keySet && (
            <label className="flex items-center gap-1 text-[11px] text-muted-foreground">
              <input type="checkbox" checked={clearKey} onChange={(e) => setClearKey(e.target.checked)} /> Remove the stored key
            </label>
          )}
        </label>
        <div className="space-y-1 text-xs sm:col-span-2">
          <label className="flex items-center gap-1.5">
            <input type="checkbox" checked={engineDefault} onChange={(e) => setEngineDefault(e.target.checked)} data-testid="le-engine-default" />
            Engine default (send whatever model is asked for; no list)
          </label>
          {!engineDefault && (
            <textarea
              className="min-h-[60px] w-full rounded-md border bg-background p-2 font-mono text-xs outline-none focus:border-primary"
              value={models}
              onChange={(e) => setModels(e.target.value)}
              placeholder="One model per line"
              data-testid="le-models"
            />
          )}
        </div>
        <label className="space-y-1 text-xs">
          <span>Timeout (seconds, optional)</span>
          <input className={field} inputMode="numeric" value={timeout} onChange={(e) => setTimeoutS(e.target.value.replace(/[^0-9]/g, ""))} data-testid="le-timeout" />
        </label>
        {kind === "image" && (
          <label className="space-y-1 text-xs">
            <span>Model while /uncensored is on</span>
            <input className={field} value={uncensored} onChange={(e) => setUncensored(e.target.value)} data-testid="le-uncensored" />
          </label>
        )}
      </div>
      {checked && (
        <div className={checked.ok ? "text-xs text-success" : "text-xs text-destructive"} data-testid="le-check-result">
          {checked.ok ? (checked.models?.length ? `Answers. Models: ${checked.models.slice(0, 12).join(", ")}${checked.models.length > 12 ? "…" : ""}` : "Answers.") : `No: ${checked.error}`}
          {checked.ok && !!checked.models?.length && !engineDefault && !models.trim() && (
            <button type="button" className="ml-2 text-primary hover:underline" onClick={() => setModels((checked.models ?? []).join("\n"))}>
              Use these
            </button>
          )}
        </div>
      )}
      {error && <div className="text-xs text-destructive" data-testid="le-error">{error}</div>}
      <div className="flex gap-2">
        <Button type="submit" size="sm" disabled={!url.trim() || save.isPending} data-testid="le-save">
          Save
        </Button>
        <Button type="button" size="sm" variant="outline" disabled={!url.trim() || check.isPending} onClick={() => check.mutate()} data-testid="le-check">
          Check
        </Button>
        <Button type="button" size="sm" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

function ToolDefaults({ le }: { le: LocalEngines }) {
  const t = useToggle();
  const kinds = (Object.keys(le.tools) as EngineKind[]).filter((k) => le.tools[k]);
  return (
    <section className="space-y-2" data-testid="le-tools">
      <div>
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Agent tools</h3>
        <p className="text-xs text-muted-foreground">
          One tool per kind. A call goes to the kind's default engine, or to the engine serving the model it names. The agent uses these from the tools release; until then images
          still come from Settings → Images.
        </p>
      </div>
      <div className="grid gap-2 sm:grid-cols-2">
        {kinds.map((k) => {
          const choices = toolChoices(le, k);
          return (
            <label key={k} className="space-y-1 text-xs">
              <span className="font-mono">{le.tools[k]}</span> <span className="text-muted-foreground">({KIND_LABELS[k]})</span>
              <select
                className={field}
                value={le.defaults[k] ?? ""}
                disabled={choices.length === 0}
                onChange={(e) => t.mutate(() => localEnginesApi.tools({ defaults: { [k]: e.target.value } }))}
                data-testid={`le-default-${k}`}
              >
                {choices.length === 0 && <option value="">No engine exposed as this tool</option>}
                {choices.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.title}
                  </option>
                ))}
              </select>
            </label>
          );
        })}
      </div>
    </section>
  );
}
