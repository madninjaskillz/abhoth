// router.json as forms (D151): the model names the gateway serves (cards, served id, context,
// vision, tools, reasoning and effort levels), the upstreams (cards' engines), the Claude block and
// slots, plus the file as text. Every save is checked by the router's own loader, reviewed as a
// diff, then backed up, written atomically, audited and hot-reloaded.
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Boxes, Loader2, Network, Sparkles } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { RouterApiError, routerApi } from "../routerApi";
import {
  claudeForm, claudeValue, deployments, gatewayText, modelBadges, modelForm, modelValue, obj, str, upstreamForm, upstreamValue, validateClaude, validateModel,
  validateUpstream, withDeployment, type ClaudeForm, type Errors, type GatewayDoc, type ModelForm, type Obj, type UpstreamForm,
} from "./formModel";
import { Check2, Field, FormDialog, Grid, ReviewDialog, RowActions, SectionHead, Select, Text, area, errText, hasErrors, type Review } from "./formKit";

const KEY = ["router-gateway"];

function useGatewayFlow(doc: GatewayDoc | undefined) {
  const qc = useQueryClient();
  const [pending, setPending] = useState<{ text: string; review: Review; done?: () => void } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const prepare = useMutation({
    mutationFn: async ({ text }: { text: string; done?: () => void }) => ({ text, check: await routerApi.gatewayCheck(text) }),
    onSuccess: (r, v) => {
      setError(null);
      setPending({ text: r.text, review: { ok: r.check.ok, diff: r.check.diff, problems: r.check.problems, consequences: r.check.consequences }, done: v.done });
    },
    onError: (e) => setError(errText(e)),
  });
  const save = useMutation({
    mutationFn: ({ reason }: { reason: string }) => routerApi.gatewaySave(pending!.text, doc!.rev, reason),
    onSuccess: () => {
      pending?.done?.();
      setPending(null);
      setError(null);
      void qc.invalidateQueries({ queryKey: KEY });
    },
    onError: (e) => setError(e instanceof RouterApiError && e.status === 412 ? "router.json changed since you loaded it: reload and make the change again." : errText(e)),
  });
  return { pending, setPending, error, prepare, save };
}

export function GatewayForms() {
  const doc = useQuery({ queryKey: KEY, queryFn: routerApi.gateway, refetchOnWindowFocus: false });
  const flow = useGatewayFlow(doc.data);
  if (doc.isLoading) return <Loader2 className="m-4 size-4 animate-spin text-muted-foreground" />;
  if (doc.isError) return <div className="p-4 text-xs text-destructive">{errText(doc.error)}</div>;
  const d = doc.data!;
  const submit = (text: string, done?: () => void) => flow.prepare.mutate({ text, done });
  const data = d.data;
  return (
    <div className="min-h-0 flex-1 overflow-y-auto" data-testid="gateway-forms">
      <div className="space-y-1 border-b px-4 py-2 text-xs text-muted-foreground">
        <div><span className="font-mono">{d.path}</span>{!d.writable && <span className="text-destructive"> (read-only on disk: saves will fail)</span>}</div>
        {d.overlay && <div>The control plane adds the cards' live engines and on-demand settings on top of this file when it loads it; what you edit here is the file itself.</div>}
        {d.error && <div className="text-destructive">The router could not load the file last time: {d.error}</div>}
      </div>
      {!data ? (
        <div className="p-4 text-xs text-destructive">router.json is not valid JSON; fix it in the Text tab.</div>
      ) : null}
      <Tabs defaultValue={data ? "models" : "text"} className="flex flex-col">
        <div className="border-b px-4 py-2">
          <TabsList>
            {data && <TabsTrigger value="models" data-testid="gateway-models"><Boxes className="mr-1 size-3.5" />Models</TabsTrigger>}
            {data && <TabsTrigger value="upstreams" data-testid="gateway-upstreams"><Network className="mr-1 size-3.5" />Upstreams</TabsTrigger>}
            {data && <TabsTrigger value="claude" data-testid="gateway-claude"><Sparkles className="mr-1 size-3.5" />Claude & slots</TabsTrigger>}
            <TabsTrigger value="text" data-testid="gateway-text">Text</TabsTrigger>
          </TabsList>
        </div>
        <div className="p-4">
          {data && <TabsContent value="models"><ModelsSection data={data} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>}
          {data && <TabsContent value="upstreams"><UpstreamsSection data={data} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>}
          {data && <TabsContent value="claude"><ClaudeSection data={data} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>}
          <TabsContent value="text"><TextSection doc={d} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>
        </div>
      </Tabs>
      {flow.pending && (
        <ReviewDialog file="router.json" review={flow.pending.review} busy={flow.save.isPending} error={flow.error} onClose={() => flow.setPending(null)} onSave={(reason) => flow.save.mutate({ reason })} />
      )}
    </div>
  );
}

interface SectionProps { data: Obj; submit: (text: string, done?: () => void) => void; busy: boolean; error: string | null }

function ModelsSection({ data, submit, busy, error }: SectionProps) {
  const list = deployments(data);
  const upstreams = Object.keys(obj(data.upstreams));
  const [editing, setEditing] = useState<{ form: ModelForm; index: number } | null>(null);
  return (
    <section className="space-y-2">
      <SectionHead title="Model names" blurb="What clients ask for, which cards answer it (a name on several cards is balanced), and what the model can do (shown to apps in router mode)." onAdd={() => setEditing({ form: modelForm(undefined), index: -1 })} addLabel="Add model" testid="model-add" />
      {list.length === 0 && <div className="text-xs text-muted-foreground">No models yet.</div>}
      <div className="divide-y rounded-md border">
        {list.map((m, i) => (
          <div key={`${str(m.name)}-${i}`} className="flex items-center gap-2 px-3 py-2 text-xs" data-testid="model-row" data-model={str(m.name)}>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-1.5">
                <span className="font-mono text-sm font-medium">{str(m.name)}</span>
                {modelBadges(m).map((b) => <Badge key={b} variant="outline">{b}</Badge>)}
              </div>
              <div className="text-muted-foreground">
                {m.relay === true ? "on-demand relay" : `cards ${(Array.isArray(m.cards) ? m.cards : []).map((c) => str(c)).join(", ")}`} · serves {str(m.servedId)}
              </div>
            </div>
            <RowActions testid="model" onEdit={() => setEditing({ form: modelForm(m), index: i })} onRemove={() => submit(withDeployment(data, i, null))} />
          </div>
        ))}
      </div>
      {editing && (
        <ModelDialog
          state={editing}
          upstreams={upstreams}
          hasRelay={!!data.relay}
          busy={busy}
          error={error}
          onClose={() => setEditing(null)}
          onSubmit={(f) => submit(withDeployment(data, editing.index, modelValue(f, editing.index >= 0 ? list[editing.index] : undefined)), () => setEditing(null))}
        />
      )}
    </section>
  );
}

function ModelDialog({ state, upstreams, hasRelay, busy, error, onClose, onSubmit }: { state: { form: ModelForm; index: number }; upstreams: string[]; hasRelay: boolean; busy: boolean; error: string | null; onClose: () => void; onSubmit: (f: ModelForm) => void }) {
  const [f, setF] = useState(state.form);
  const [errs, setErrs] = useState<Errors>({});
  const set = (k: keyof ModelForm) => (v: string) => setF({ ...f, [k]: v });
  return (
    <FormDialog
      title={state.index >= 0 ? `Model ${state.form.name}` : "New model"}
      description="One entry of the gateway's model list. The same name on more cards means more entries (or one entry with several cards)."
      onClose={onClose}
      busy={busy}
      error={error}
      testid="model-dialog"
      onSubmit={() => {
        const e = validateModel(f, upstreams, hasRelay);
        setErrs(e);
        if (!hasErrors(e)) onSubmit(f);
      }}
    >
      <Grid>
        <Field label="Name" hint="What clients send as model." error={errs.name}><Text value={f.name} onChange={set("name")} mono testid="model-name" /></Field>
        <Field label="Served id" hint="The id the engine answers to." error={errs.servedId}><Text value={f.servedId} onChange={set("servedId")} mono testid="model-served" /></Field>
        <Field label="Cards" hint={f.relay ? "Goes to the on-demand relay instead." : "The upstreams that answer it."} error={errs.cards ?? errs.relay} wide>
          <div className="flex flex-wrap gap-3" data-testid="model-cards">
            {upstreams.map((c) => (
              <Check2 key={c} checked={f.cards.includes(c)} onChange={(on) => setF({ ...f, cards: on ? [...f.cards, c] : f.cards.filter((x) => x !== c) })} label={c} testid={`model-card-${c}`} />
            ))}
            {hasRelay && <Check2 checked={f.relay} onChange={(v) => setF({ ...f, relay: v })} label="on-demand relay" />}
          </div>
        </Field>
        <Field label="Context (tokens)" hint="max_input_tokens" error={errs.contextTokens}><Text value={f.contextTokens} onChange={set("contextTokens")} mono testid="model-context" /></Field>
        <Field label="Output limit (tokens)" hint="Empty: as the context." error={errs.outputTokens}><Text value={f.outputTokens} onChange={set("outputTokens")} mono /></Field>
        <div className="flex flex-wrap gap-4 sm:col-span-2">
          <Check2 checked={f.vision} onChange={(v) => setF({ ...f, vision: v })} label="Vision (images)" testid="model-vision" />
          <Check2 checked={f.tools} onChange={(v) => setF({ ...f, tools: v })} label="Tool calling" testid="model-tools" />
          <Check2 checked={f.reasoning} onChange={(v) => setF({ ...f, reasoning: v })} label="Reasoning" testid="model-reasoning" />
        </div>
        <Field label="Effort levels" hint="e.g. low, medium, high"><Text value={f.effortLevels} onChange={set("effortLevels")} mono testid="model-effort" /></Field>
        <Field label="Thinking off means" hint="drop, none, or an effort level." error={errs.thinkingOffEffort}><Text value={f.thinkingOffEffort} onChange={set("thinkingOffEffort")} mono /></Field>
        <Field label="Weight" hint="Balancing weight among entries with this name." error={errs.weight}><Text value={f.weight} onChange={set("weight")} mono /></Field>
        <Field label="Dropped fields" hint="Request fields removed before forwarding."><Text value={f.dropParams} onChange={set("dropParams")} mono /></Field>
        <Field label="Request defaults (JSON)" hint="Added when the client did not send them." error={errs.params} wide>
          <textarea className={area} value={f.params} onChange={(e) => set("params")(e.target.value)} placeholder='e.g. {"reasoning_effort": "low"}' />
        </Field>
      </Grid>
    </FormDialog>
  );
}

function UpstreamsSection({ data, submit, busy, error }: SectionProps) {
  const ups = obj(data.upstreams);
  const [editing, setEditing] = useState<{ form: UpstreamForm; original: string | null } | null>(null);
  const [blocked, setBlocked] = useState<string | null>(null);
  return (
    <section className="space-y-2">
      <SectionHead title="Upstreams" blurb="The engine behind each card id: its URL (no /v1), lanes (parallel requests) and the file holding its key." onAdd={() => setEditing({ form: upstreamForm("", {}), original: null })} addLabel="Add upstream" testid="upstream-add" />
      {blocked && <div className="text-xs text-destructive">{blocked}</div>}
      <div className="divide-y rounded-md border">
        {Object.entries(ups).map(([card, u]) => {
          const uo = obj(u);
          return (
            <div key={card} className="flex items-center gap-2 px-3 py-2 text-xs" data-testid="upstream-row" data-card={card}>
              <span className="w-10 font-mono font-medium">{card}</span>
              <div className="min-w-0 flex-1">
                <div className="truncate font-mono">{str(uo.url)}</div>
                <div className="text-muted-foreground">{str(uo.engine) || "engine ?"} · {String(uo.lanes ?? 1)} lanes{uo.online === false ? " · offline" : ""}{uo.keyFile ? " · key file" : ""}</div>
              </div>
              <RowActions
                testid="upstream"
                onEdit={() => setEditing({ form: upstreamForm(card, u), original: card })}
                onRemove={() => {
                  const users = deployments(data).filter((m) => Array.isArray(m.cards) && m.cards.includes(card)).map((m) => str(m.name));
                  if (users.length) return setBlocked(`${card} still answers ${users.join(", ")}.`);
                  setBlocked(null);
                  submit(gatewayText(data, (d) => void delete obj(d.upstreams)[card]));
                }}
              />
            </div>
          );
        })}
      </div>
      {editing && (
        <UpstreamDialog
          state={editing}
          existing={Object.keys(ups)}
          busy={busy}
          error={error}
          onClose={() => setEditing(null)}
          onSubmit={(f) =>
            submit(
              gatewayText(data, (d) => {
                const next: Obj = {};
                for (const [k, v] of Object.entries(obj(d.upstreams))) next[k === editing.original ? f.card : k] = k === editing.original ? upstreamValue(f, v) : v;
                if (!editing.original) next[f.card] = upstreamValue(f, undefined);
                d.upstreams = next;
                if (editing.original && editing.original !== f.card)
                  for (const m of deployments(d)) if (Array.isArray(m.cards)) m.cards = m.cards.map((c) => (c === editing.original ? f.card : c));
              }),
              () => setEditing(null),
            )
          }
        />
      )}
    </section>
  );
}

function UpstreamDialog({ state, existing, busy, error, onClose, onSubmit }: { state: { form: UpstreamForm; original: string | null }; existing: string[]; busy: boolean; error: string | null; onClose: () => void; onSubmit: (f: UpstreamForm) => void }) {
  const [f, setF] = useState(state.form);
  const [errs, setErrs] = useState<Errors>({});
  const set = (k: keyof UpstreamForm) => (v: string) => setF({ ...f, [k]: v });
  return (
    <FormDialog
      title={state.original ? `Upstream ${state.original}` : "New upstream"}
      description="Keys are read from the key file at load time and never appear in router.json."
      onClose={onClose}
      busy={busy}
      error={error}
      testid="upstream-dialog"
      onSubmit={() => {
        const e = validateUpstream(f, existing, state.original);
        setErrs(e);
        if (!hasErrors(e)) onSubmit(f);
      }}
    >
      <Grid>
        <Field label="Card" error={errs.card}><Text value={f.card} onChange={set("card")} mono testid="upstream-card" /></Field>
        <Field label="URL" error={errs.url}><Text value={f.url} onChange={set("url")} mono placeholder="http://gpu-host:8094" testid="upstream-url" /></Field>
        <Field label="Engine"><Text value={f.engine} onChange={set("engine")} mono /></Field>
        <Field label="Served id"><Text value={f.servedId} onChange={set("servedId")} mono /></Field>
        <Field label="Lanes" error={errs.lanes}><Text value={f.lanes} onChange={set("lanes")} mono testid="upstream-lanes" /></Field>
        <Field label="Key file" hint="Path to a file with the engine's key." error={errs.keyFile}><Text value={f.keyFile} onChange={set("keyFile")} mono /></Field>
        <Check2 checked={f.online} onChange={(v) => setF({ ...f, online: v })} label="Online" />
      </Grid>
    </FormDialog>
  );
}

function ClaudeSection({ data, submit, busy, error }: SectionProps) {
  const [f, setF] = useState<ClaudeForm>(claudeForm(data.claude));
  const slots = obj(data.slots);
  const [slotsOn, setSlotsOn] = useState(slots.enabled === true);
  const [ttl, setTtl] = useState(typeof slots.ttlSeconds === "number" ? String(slots.ttlSeconds) : "");
  const [errs, setErrs] = useState<Errors>({});
  useEffect(() => setF(claudeForm(data.claude)), [data]);
  const set = (k: keyof ClaudeForm) => (v: string) => setF({ ...f, [k]: v });
  return (
    <section className="space-y-6">
      <div className="space-y-3">
        <SectionHead title="Claude" blurb="Claude models as routes, answered by the Claude Code CLI under the operator's own subscription (needs a router built with the claude-engine feature)." />
        <Grid>
          <Check2 checked={f.enabled} onChange={(v) => setF({ ...f, enabled: v })} label="Enabled" testid="claude-enabled" />
          <Field label="Who may use it" hint="admins (default), users, anonymous or keys:<names>." error={errs.access}>
            <Select value={["admins", "users", "anonymous"].includes(f.access) ? f.access : "keys"} onChange={(v) => setF({ ...f, access: v === "keys" ? "keys:" : v })} options={[["admins", "admins"], ["users", "signed-in users"], ["anonymous", "anyone"], ["keys", "named keys…"]]} />
          </Field>
          {f.access.startsWith("keys:") && <Field label="Keys or groups"><Text value={f.access.slice(5)} onChange={(v) => setF({ ...f, access: `keys:${v}` })} mono /></Field>}
          <Field label="Sessions at once" error={errs.maxSessions}><Text value={f.maxSessions} onChange={set("maxSessions")} mono /></Field>
          <Field label="Context (tokens)"><Text value={f.contextTokens} onChange={set("contextTokens")} mono /></Field>
          <Field label="Output limit (tokens)"><Text value={f.maxOutputTokens} onChange={set("maxOutputTokens")} mono /></Field>
          <Field label="Routes" hint="route-name = CLI model (opus, sonnet, haiku or an id), one per line." error={errs.routes} wide>
            <textarea className={area} value={f.routes} onChange={(e) => set("routes")(e.target.value)} data-testid="claude-routes" />
          </Field>
        </Grid>
      </div>
      <div className="space-y-3">
        <SectionHead title="Slots" blurb="Agent slot leases (pig-slots): whether the router keeps them, and how long an unrenewed lease lives." />
        <Grid>
          <Check2 checked={slotsOn} onChange={setSlotsOn} label="Slots on" testid="slots-enabled" />
          <Field label="Lease TTL (seconds)"><Text value={ttl} onChange={setTtl} mono /></Field>
        </Grid>
      </div>
      {error && <div className="text-xs text-destructive">{error}</div>}
      <Button
        size="sm"
        disabled={busy}
        data-testid="claude-review"
        onClick={() => {
          const e = validateClaude(f);
          setErrs(e);
          if (hasErrors(e)) return;
          submit(
            gatewayText(data, (d) => {
              const c = claudeValue(f, d.claude);
              if (c.enabled || d.claude) d.claude = c;
              const s: Obj = { ...obj(d.slots), enabled: slotsOn };
              const n = Number(ttl);
              if (ttl.trim() && Number.isFinite(n)) s.ttlSeconds = n;
              d.slots = s;
            }),
          );
        }}
      >
        {busy && <Loader2 className="animate-spin" />} Review changes
      </Button>
    </section>
  );
}

function TextSection({ doc, submit, busy, error }: { doc: GatewayDoc; submit: (text: string) => void; busy: boolean; error: string | null }) {
  const [text, setText] = useState(doc.text);
  useEffect(() => setText(doc.text), [doc.text]);
  return (
    <section className="space-y-2">
      <SectionHead title="router.json" blurb="The whole file. Review checks it with the router's own loader before anything is written." />
      <textarea className={`${area} min-h-[420px]`} value={text} onChange={(e) => setText(e.target.value)} spellCheck={false} data-testid="gateway-textarea" />
      {error && <div className="text-xs text-destructive">{error}</div>}
      <div className="flex gap-2">
        <Button size="sm" disabled={busy || text === doc.text} onClick={() => submit(text)} data-testid="gateway-text-review">{busy && <Loader2 className="animate-spin" />} Review changes</Button>
        <Button size="sm" variant="ghost" disabled={text === doc.text} onClick={() => setText(doc.text)}>Discard</Button>
      </div>
    </section>
  );
}
