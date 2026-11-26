// router.toml as forms (D151): servers (and how the router reaches them), their cards, engines
// with their profiles (lanes per card), and layouts. A form's Save sends its edits to the router,
// which applies them to the file (comments kept) and checks the result; the review dialog shows
// the diff, and Save writes it through the same audited save as the text editor.
import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Cpu, Layers, Loader2, Server } from "lucide-react";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { RouterApiError, routerApi } from "../routerApi";
import {
  AGENT_MODES, DRIVERS, EXEC_KINDS, cardForm, cardRuns, cardValue, engineForm, engineValue, ids, layoutForm, layoutValue, obj, removeEdits,
  sectionEdits, serverForm, serverValue, str, usedBy, validateCard, validateEngine, validateLayout, validateServer,
  type CardForm, type ConfigData, type EngineForm, type Errors, type FormEdit, type LayoutForm, type Obj, type ServerForm,
} from "./formModel";
import { Check2, Field, FormDialog, Grid, ReviewDialog, RowActions, SectionHead, Select, Text, area, errText, hasErrors, type Review } from "./formKit";

const KEY = ["router-config-json"];

/** Turn edits into a checked text, review it, save it. */
export function useEditFlow() {
  const qc = useQueryClient();
  const [pending, setPending] = useState<{ text: string; rev: string; review: Review; done?: () => void } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const prepare = useMutation({
    mutationFn: async ({ edits, rev }: { edits: FormEdit[]; rev: string; done?: () => void }) => ({ ...(await routerApi.configEdit(edits)), rev }),
    onSuccess: (r, v) => {
      setError(null);
      setPending({ text: r.text, rev: v.rev, review: { ok: r.check.ok, diff: r.check.diff, problems: r.check.problems, consequences: r.check.consequences }, done: v.done });
    },
    onError: (e) => setError(errText(e)),
  });
  const save = useMutation({
    mutationFn: ({ reason }: { reason: string }) => routerApi.save(pending!.text, pending!.rev, reason),
    onSuccess: () => {
      pending?.done?.();
      setPending(null);
      setError(null);
      void qc.invalidateQueries({ queryKey: KEY });
      void qc.invalidateQueries({ queryKey: ["router-config"] });
      void qc.invalidateQueries({ queryKey: ["router-inventory"] });
    },
    onError: (e) => setError(e instanceof RouterApiError && e.status === 412 ? "router.toml changed since you loaded it: reload the page and make the change again." : errText(e)),
  });
  return { pending, setPending, error, setError, prepare, save };
}

export function RouterSetupForms() {
  const doc = useQuery({ queryKey: KEY, queryFn: routerApi.configJson, refetchOnWindowFocus: false });
  const flow = useEditFlow();
  if (doc.isLoading) return <Loader2 className="m-4 size-4 animate-spin text-muted-foreground" />;
  if (doc.isError) return <div className="p-4 text-xs text-destructive">{errText(doc.error)}</div>;
  const d = doc.data!;
  if (!d.data) return <div className="p-4 text-xs text-destructive">router.toml does not parse ({d.parseError}); fix it in the Advanced tab.</div>;
  const submit = (edits: FormEdit[], done?: () => void) => flow.prepare.mutate({ edits, rev: d.rev, done });
  return (
    <div className="min-h-0 flex-1 overflow-y-auto" data-testid="router-setup">
      {(d.error || d.includes.length > 0) && (
        <div className="space-y-1 border-b px-4 py-2 text-xs">
          {d.error && <div className="text-destructive">The file on disk does not load: {d.error}</div>}
          {d.includes.length > 0 && <div className="text-muted-foreground">Only {d.path} is shown here; sections in its included files ({d.includes.join(", ")}) are edited in Advanced.</div>}
        </div>
      )}
      <Tabs defaultValue="servers" className="flex flex-col">
        <div className="border-b px-4 py-2">
          <TabsList>
            <TabsTrigger value="servers" data-testid="setup-servers"><Server className="mr-1 size-3.5" />Servers & cards</TabsTrigger>
            <TabsTrigger value="engines" data-testid="setup-engines"><Cpu className="mr-1 size-3.5" />Engines</TabsTrigger>
            <TabsTrigger value="layouts" data-testid="setup-layouts"><Layers className="mr-1 size-3.5" />Layouts</TabsTrigger>
          </TabsList>
        </div>
        <div className="p-4">
          <TabsContent value="servers"><ServersSection d={d} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>
          <TabsContent value="engines"><EnginesSection d={d} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>
          <TabsContent value="layouts"><LayoutsSection d={d} submit={submit} busy={flow.prepare.isPending} error={flow.error} /></TabsContent>
        </div>
      </Tabs>
      {flow.pending && (
        <ReviewDialog
          file="router.toml"
          review={flow.pending.review}
          busy={flow.save.isPending}
          error={flow.error}
          onClose={() => flow.setPending(null)}
          onSave={(reason) => flow.save.mutate({ reason })}
        />
      )}
    </div>
  );
}

type Submit = (edits: FormEdit[], done?: () => void) => void;
interface SectionProps { d: ConfigData; submit: Submit; busy: boolean; error: string | null }

// ── servers & cards ──────────────────────────────────────────────────────────────────────────

function ServersSection({ d, submit, busy, error }: SectionProps) {
  const data = d.data as Obj;
  const servers = obj(data.servers);
  const cards = obj(data.cards);
  const [server, setServer] = useState<{ form: ServerForm; original: string | null } | null>(null);
  const [card, setCard] = useState<{ form: CardForm; original: string | null } | null>(null);
  const [blocked, setBlocked] = useState<string | null>(null);
  const remove = (section: "servers" | "cards", id: string) => {
    const users = usedBy(data, section, id);
    if (users.length) return setBlocked(`${id} is still used by ${users.join(", ")}.`);
    setBlocked(null);
    submit(removeEdits(section, id));
  };
  return (
    <div className="space-y-6">
      <section className="space-y-2">
        <SectionHead title="Servers" blurb="The machines with GPUs, and how the router runs commands on them (SSH to a forced-command account, or locally)." onAdd={() => setServer({ form: serverForm("", {}), original: null })} addLabel="Add server" testid="server-add" />
        {blocked && <div className="text-xs text-destructive">{blocked}</div>}
        {Object.keys(servers).length === 0 && <div className="text-xs text-muted-foreground">No servers yet.</div>}
        {Object.entries(servers).map(([sid, s]) => {
          const so = obj(s);
          const own = Object.entries(cards).filter(([, c]) => str(obj(c).server) === sid);
          return (
            <div key={sid} className="rounded-md border" data-testid="server-row" data-server={sid}>
              <div className="flex items-center gap-2 border-b px-3 py-2">
                <Server className="size-4 text-muted-foreground" />
                <div className="min-w-0 flex-1">
                  <div className="text-sm font-medium">{str(so.title) || sid} <span className="font-mono text-xs text-muted-foreground">{sid}</span></div>
                  <div className="truncate text-xs text-muted-foreground">
                    {str(so.exec) === "local" ? "local commands" : `ssh ${str(obj(so.ssh).user) || "?"}@${str(so.host)}${obj(so.ssh).port ? `:${obj(so.ssh).port}` : ""}`}
                  </div>
                </div>
                <RowActions testid="server" onEdit={() => setServer({ form: serverForm(sid, s), original: sid })} onRemove={() => remove("servers", sid)} />
              </div>
              <div className="space-y-1 p-2">
                {own.map(([cid, c]) => <CardRow key={cid} id={cid} c={obj(c)} data={data} onEdit={() => setCard({ form: cardForm(cid, c), original: cid })} onRemove={() => remove("cards", cid)} />)}
                <button className="w-full rounded px-2 py-1 text-left text-xs text-primary hover:bg-accent/50" onClick={() => setCard({ form: { ...cardForm("", {}), server: sid }, original: null })} data-testid="card-add">
                  + Add a card to {str(so.title) || sid}
                </button>
              </div>
            </div>
          );
        })}
        {Object.entries(cards).filter(([, c]) => !servers[str(obj(c).server)]).map(([cid, c]) => (
          <CardRow key={cid} id={cid} c={obj(c)} data={data} onEdit={() => setCard({ form: cardForm(cid, c), original: cid })} onRemove={() => remove("cards", cid)} />
        ))}
      </section>
      {server && (
        <ServerDialog
          state={server}
          existing={ids(data, "servers")}
          busy={busy}
          error={error}
          onClose={() => setServer(null)}
          onSubmit={(f) => submit(sectionEdits("servers", f.id, server.original, serverValue(f, server.original ? servers[server.original] : undefined)), () => setServer(null))}
        />
      )}
      {card && (
        <CardDialog
          state={card}
          existing={ids(data, "cards")}
          servers={ids(data, "servers")}
          data={data}
          busy={busy}
          error={error}
          onClose={() => setCard(null)}
          onSubmit={(f) => submit(sectionEdits("cards", f.id, card.original, cardValue(f, card.original ? cards[card.original] : undefined)), () => setCard(null))}
        />
      )}
    </div>
  );
}

function CardRow({ id, c, data, onEdit, onRemove }: { id: string; c: Obj; data: Obj; onEdit: () => void; onRemove: () => void }) {
  const runs = cardRuns(data, id);
  return (
    <div className="flex items-start gap-2 rounded px-2 py-1.5 hover:bg-accent/30" data-testid="card-row" data-card={id}>
      <Cpu className="mt-0.5 size-3.5 text-muted-foreground" />
      <div className="min-w-0 flex-1 text-xs">
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="font-mono font-medium">{id}</span>
          <span>{str(c.label)}</span>
          {typeof c.vram_gib === "number" && <Badge variant="outline">{c.vram_gib} GiB</Badge>}
          {c.managed === false && <Badge variant="outline">not managed</Badge>}
          {c.on_demand === true && <Badge variant="outline">on demand</Badge>}
        </div>
        {runs.length > 0 && (
          <div className="text-muted-foreground">
            {runs.map((r) => `${r.layout}: ${r.engine}/${r.profile}${r.lanes ? ` (${r.lanes} lanes)` : ""} ${r.model}`).join(" · ")}
          </div>
        )}
      </div>
      <RowActions testid="card" onEdit={onEdit} onRemove={onRemove} />
    </div>
  );
}

function ServerDialog({ state, existing, busy, error, onClose, onSubmit }: { state: { form: ServerForm; original: string | null }; existing: string[]; busy: boolean; error: string | null; onClose: () => void; onSubmit: (f: ServerForm) => void }) {
  const [f, setF] = useState(state.form);
  const [errs, setErrs] = useState<Errors>({});
  const set = (k: keyof ServerForm) => (v: string) => setF({ ...f, [k]: v });
  return (
    <FormDialog
      title={state.original ? `Server ${state.original}` : "New server"}
      description="Where the GPUs are and how the router reaches them. Keys are never put here: name a secret from [secrets] (secret:<id>)."
      onClose={onClose}
      busy={busy}
      error={error}
      testid="server-dialog"
      onSubmit={() => {
        const e = validateServer(f, existing, state.original);
        setErrs(e);
        if (!hasErrors(e)) onSubmit(f);
      }}
    >
      <Grid>
        <Field label="Id" hint="Used in cards and the audit, e.g. gpu-host." error={errs.id}><Text value={f.id} onChange={set("id")} mono testid="server-id" /></Field>
        <Field label="Title"><Text value={f.title} onChange={set("title")} testid="server-title" /></Field>
        <Field label="Host" hint="Name or address the router connects to." error={errs.host}><Text value={f.host} onChange={set("host")} mono testid="server-host" /></Field>
        <Field label="Commands run"><Select value={f.exec} onChange={set("exec")} options={EXEC_KINDS.map((k) => [k, k === "ssh" ? "over SSH" : "on the router's host"])} testid="server-exec" /></Field>
        {f.exec === "ssh" && (
          <>
            <Field label="SSH user" error={errs.sshUser}><Text value={f.sshUser} onChange={set("sshUser")} mono testid="server-ssh-user" /></Field>
            <Field label="SSH port" hint="Empty: 22." error={errs.sshPort}><Text value={f.sshPort} onChange={set("sshPort")} mono testid="server-ssh-port" /></Field>
            <Field label="SSH key" hint="secret:<id> from [secrets]." error={errs.sshKey}><Text value={f.sshKey} onChange={set("sshKey")} mono placeholder="secret:ssh_gpu" testid="server-ssh-key" /></Field>
            <Field label="Host key" hint="SHA256:… (ssh-keyscan | ssh-keygen -lf -)." error={errs.sshHostKey}><Text value={f.sshHostKey} onChange={set("sshHostKey")} mono testid="server-host-key" /></Field>
          </>
        )}
        <Field label="Variables" hint="name = value, one per line ({var:name} in templates)." error={errs.vars} wide>
          <textarea className={area} value={f.vars} onChange={(e) => set("vars")(e.target.value)} data-testid="server-vars" />
        </Field>
        <Field label="Locks" hint="Files that block a switch while they exist, one per line." wide>
          <textarea className={area} value={f.locks} onChange={(e) => set("locks")(e.target.value)} data-testid="server-locks" />
        </Field>
      </Grid>
    </FormDialog>
  );
}

function CardDialog({ state, existing, servers, data, busy, error, onClose, onSubmit }: { state: { form: CardForm; original: string | null }; existing: string[]; servers: string[]; data: Obj; busy: boolean; error: string | null; onClose: () => void; onSubmit: (f: CardForm) => void }) {
  const [f, setF] = useState(state.form);
  const [errs, setErrs] = useState<Errors>({});
  const set = (k: keyof CardForm) => (v: string) => setF({ ...f, [k]: v });
  const runs = state.original ? cardRuns(data, state.original) : [];
  return (
    <FormDialog
      title={state.original ? `Card ${state.original}` : "New card"}
      description="A GPU the router manages, by its UUID. What it runs and with how many lanes comes from the layouts and the engines' profiles."
      onClose={onClose}
      busy={busy}
      error={error}
      testid="card-dialog"
      onSubmit={() => {
        const e = validateCard(f, existing, state.original, servers);
        setErrs(e);
        if (!hasErrors(e)) onSubmit(f);
      }}
    >
      <Grid>
        <Field label="Id" hint="A short name clients see (A, B…)." error={errs.id}><Text value={f.id} onChange={set("id")} mono testid="card-id" /></Field>
        <Field label="Server" error={errs.server}><Select value={f.server} onChange={set("server")} options={["", ...servers]} testid="card-server" /></Field>
        <Field label="GPU UUID" hint="nvidia-smi -L" error={errs.gpuUuid} wide><Text value={f.gpuUuid} onChange={set("gpuUuid")} mono placeholder="GPU-…" testid="card-uuid" /></Field>
        <Field label="Label"><Text value={f.label} onChange={set("label")} testid="card-label" /></Field>
        <Field label="Memory (GiB)" error={errs.vramGib}><Text value={f.vramGib} onChange={set("vramGib")} mono testid="card-vram" /></Field>
        <Field label="Reserve (GiB)" hint="Kept free when placing models." error={errs.reserveGib}><Text value={f.reserveGib} onChange={set("reserveGib")} mono /></Field>
        <Field label="CPUs" hint="Pinned CPUs, e.g. 0-23." error={errs.cpus}><Text value={f.cpus} onChange={set("cpus")} mono /></Field>
        <Check2 checked={f.managed} onChange={(v) => setF({ ...f, managed: v })} label="Managed (the router starts and stops engines here)" testid="card-managed" />
        <Check2 checked={f.onDemand} onChange={(v) => setF({ ...f, onDemand: v })} label="May switch on demand" />
      </Grid>
      {runs.length > 0 && (
        <div className="rounded-md border p-2 text-xs" data-testid="card-runs">
          <div className="mb-1 font-medium">In the layouts</div>
          {runs.map((r) => (
            <div key={r.layout} className="flex gap-2"><span className="font-mono">{r.layout}</span><span className="text-muted-foreground">{r.engine} / {r.profile}{r.lanes ? ` · ${r.lanes} lanes` : ""} · {r.model}</span></div>
          ))}
        </div>
      )}
    </FormDialog>
  );
}

// ── engines ──────────────────────────────────────────────────────────────────────────────────

function EnginesSection({ d, submit, busy, error }: SectionProps) {
  const data = d.data as Obj;
  const engines = obj(data.engines);
  const [editing, setEditing] = useState<{ form: EngineForm; original: string | null } | null>(null);
  const [blocked, setBlocked] = useState<string | null>(null);
  return (
    <section className="space-y-2">
      <SectionHead title="Engines" blurb="Inference servers the router starts on cards. Each profile is one way to run it: lanes (parallel slots), context and memory." onAdd={() => setEditing({ form: engineForm("", { driver: "docker" }), original: null })} addLabel="Add engine" testid="engine-add" />
      {blocked && <div className="text-xs text-destructive">{blocked}</div>}
      {Object.entries(engines).map(([eid, e]) => {
        const eo = obj(e);
        return (
          <div key={eid} className="flex items-start gap-2 rounded-md border px-3 py-2" data-testid="engine-row" data-engine={eid}>
            <div className="min-w-0 flex-1 text-xs">
              <div className="text-sm font-medium">{str(eo.title) || eid} <span className="font-mono text-xs text-muted-foreground">{eid} · {str(eo.driver)}</span></div>
              <div className="mt-1 flex flex-wrap gap-1">
                {Object.entries(obj(eo.profiles)).map(([pid, p]) => <Badge key={pid} variant="outline">{pid}: {String(obj(p).lanes ?? "?")} lanes</Badge>)}
              </div>
            </div>
            <RowActions
              testid="engine"
              onEdit={() => setEditing({ form: engineForm(eid, e), original: eid })}
              onRemove={() => {
                const users = usedBy(data, "engines", eid);
                if (users.length) return setBlocked(`${eid} is still used by ${users.join(", ")}.`);
                setBlocked(null);
                submit(removeEdits("engines", eid));
              }}
            />
          </div>
        );
      })}
      <p className="text-xs text-muted-foreground">How an engine starts (its command line, health checks, hooks) is edited in Advanced; the form keeps it as it is.</p>
      {editing && (
        <EngineDialog
          state={editing}
          existing={ids(data, "engines")}
          busy={busy}
          error={error}
          onClose={() => setEditing(null)}
          onSubmit={(f) => submit(sectionEdits("engines", f.id, editing.original, engineValue(f, editing.original ? engines[editing.original] : undefined)), () => setEditing(null))}
        />
      )}
    </section>
  );
}

function EngineDialog({ state, existing, busy, error, onClose, onSubmit }: { state: { form: EngineForm; original: string | null }; existing: string[]; busy: boolean; error: string | null; onClose: () => void; onSubmit: (f: EngineForm) => void }) {
  const [f, setF] = useState(state.form);
  const [errs, setErrs] = useState<Errors>({});
  const set = (k: keyof EngineForm) => (v: string) => setF({ ...f, [k]: v });
  const setP = (i: number, k: string, v: string) => setF({ ...f, profiles: f.profiles.map((p, j) => (j === i ? { ...p, [k]: v } : p)) });
  return (
    <FormDialog
      title={state.original ? `Engine ${state.original}` : "New engine"}
      description="The engine's identity and profiles. Its start command, health checks and hooks stay as they are in the file."
      onClose={onClose}
      busy={busy}
      error={error}
      testid="engine-dialog"
      onSubmit={() => {
        const e = validateEngine(f, existing, state.original);
        setErrs(e);
        if (!hasErrors(e)) onSubmit(f);
      }}
    >
      <Grid>
        <Field label="Id" error={errs.id}><Text value={f.id} onChange={set("id")} mono testid="engine-id" /></Field>
        <Field label="Title"><Text value={f.title} onChange={set("title")} /></Field>
        <Field label="Driver"><Select value={f.driver} onChange={set("driver")} options={DRIVERS} /></Field>
        <Field label="Served model id" hint="What the engine answers to."><Text value={f.servedId} onChange={set("servedId")} mono /></Field>
        <Field label="Image"><Text value={f.image} onChange={set("image")} mono /></Field>
        <Field label="Container"><Text value={f.container} onChange={set("container")} mono placeholder="name-{card_lc}" /></Field>
        <Field label="Port" hint="One port, or CARD = port per line." error={errs.port}>
          <textarea className={area} value={f.port} onChange={(e) => set("port")(e.target.value)} data-testid="engine-port" />
        </Field>
        <Field label="Max context (tokens)" error={errs.maxContext}><Text value={f.maxContext} onChange={set("maxContext")} mono /></Field>
      </Grid>
      <div className="space-y-1.5">
        <div className="flex items-center justify-between text-xs font-medium">
          <span>Profiles (lanes per card)</span>
          <button className="text-primary" onClick={() => setF({ ...f, profiles: [...f.profiles, { id: "", title: "", lanes: "1", ctx: "", vramGib: "", build: "" }] })} data-testid="profile-add">+ Add profile</button>
        </div>
        <div className="grid grid-cols-[1.2fr_1.2fr_0.6fr_0.9fr_0.7fr_0.8fr_auto] gap-1 text-[11px] text-muted-foreground">
          <span>Id</span><span>Title</span><span>Lanes</span><span>Context</span><span>GiB</span><span>Build</span><span />
        </div>
        {f.profiles.map((p, i) => (
          <div key={i} className="space-y-0.5" data-testid="profile-row">
            <div className="grid grid-cols-[1.2fr_1.2fr_0.6fr_0.9fr_0.7fr_0.8fr_auto] gap-1">
              <Text value={p.id} onChange={(v) => setP(i, "id", v)} mono />
              <Text value={p.title} onChange={(v) => setP(i, "title", v)} />
              <Text value={p.lanes} onChange={(v) => setP(i, "lanes", v)} mono testid="profile-lanes" />
              <Text value={p.ctx} onChange={(v) => setP(i, "ctx", v)} mono />
              <Text value={p.vramGib} onChange={(v) => setP(i, "vramGib", v)} mono />
              <Text value={p.build} onChange={(v) => setP(i, "build", v)} mono />
              <button className="px-1 text-xs text-muted-foreground hover:text-destructive" onClick={() => setF({ ...f, profiles: f.profiles.filter((_, j) => j !== i) })} aria-label="Remove profile">✕</button>
            </div>
            {["id", "lanes", "ctx", "vramGib"].map((k) => errs[`profiles.${i}.${k}`] && <div key={k} className="text-[11px] text-destructive">{errs[`profiles.${i}.${k}`]}</div>)}
          </div>
        ))}
      </div>
    </FormDialog>
  );
}

// ── layouts ──────────────────────────────────────────────────────────────────────────────────

function LayoutsSection({ d, submit, busy, error }: SectionProps) {
  const data = d.data as Obj;
  const layouts = obj(data.layouts);
  const cardIds = ids(data, "cards");
  const [editing, setEditing] = useState<{ form: LayoutForm; original: string | null } | null>(null);
  return (
    <section className="space-y-2">
      <SectionHead title="Layouts" blurb="What each card runs, as a set you switch between. Saving a layout never switches: switching is a plan with guards on the Inference page." onAdd={() => setEditing({ form: layoutForm("", {}, cardIds), original: null })} addLabel="Add layout" testid="layout-add" />
      {Object.entries(layouts).map(([lid, l]) => {
        const lo = obj(l);
        return (
          <div key={lid} className="flex items-start gap-2 rounded-md border px-3 py-2" data-testid="layout-row" data-layout={lid}>
            <div className="min-w-0 flex-1 text-xs">
              <div className="text-sm font-medium">{str(lo.title) || lid} <span className="font-mono text-xs text-muted-foreground">{lid}</span>{lo.hidden === true && <Badge variant="outline" className="ml-1">hidden</Badge>}</div>
              <div className="text-muted-foreground">
                {Object.entries(obj(lo.cards)).map(([c, s]) => `${c}: ${typeof s === "string" ? s : `${str(obj(s).engine)}/${str(obj(s).profile)} ${str(obj(s).model)}`}`).join(" · ")}
              </div>
            </div>
            <RowActions testid="layout" onEdit={() => setEditing({ form: layoutForm(lid, l, cardIds), original: lid })} onRemove={() => submit(removeEdits("layouts", lid))} />
          </div>
        );
      })}
      {editing && (
        <LayoutDialog
          state={editing}
          existing={ids(data, "layouts")}
          data={data}
          busy={busy}
          error={error}
          onClose={() => setEditing(null)}
          onSubmit={(f) => submit(sectionEdits("layouts", f.id, editing.original, layoutValue(f, editing.original ? layouts[editing.original] : undefined)), () => setEditing(null))}
        />
      )}
    </section>
  );
}

function LayoutDialog({ state, existing, data, busy, error, onClose, onSubmit }: { state: { form: LayoutForm; original: string | null }; existing: string[]; data: Obj; busy: boolean; error: string | null; onClose: () => void; onSubmit: (f: LayoutForm) => void }) {
  const [f, setF] = useState(state.form);
  const [errs, setErrs] = useState<Errors>({});
  const engines = obj(data.engines);
  const models = Object.keys(obj(data.models));
  const setSlot = (c: string, patch: Partial<LayoutForm["cards"][string]>) => setF({ ...f, cards: { ...f.cards, [c]: { ...f.cards[c], ...patch } } });
  return (
    <FormDialog
      title={state.original ? `Layout ${state.original}` : "New layout"}
      description="What each card runs in this layout. Saving changes the definition only; if it is the active layout, the Inference page shows drift and a plan."
      onClose={onClose}
      busy={busy}
      error={error}
      testid="layout-dialog"
      onSubmit={() => {
        const e = validateLayout(f, existing, state.original, data);
        setErrs(e);
        if (!hasErrors(e)) onSubmit(f);
      }}
    >
      <Grid>
        <Field label="Id" error={errs.id}><Text value={f.id} onChange={(v) => setF({ ...f, id: v })} mono testid="layout-id" /></Field>
        <Field label="Title"><Text value={f.title} onChange={(v) => setF({ ...f, title: v })} /></Field>
        <Field label="Description" wide><Text value={f.description} onChange={(v) => setF({ ...f, description: v })} /></Field>
        <Field label="Agent mode" hint="Carried for clients (the profiles shim)."><Select value={f.mode} onChange={(v) => setF({ ...f, mode: v })} options={AGENT_MODES.map((m) => [m, m || "(none)"])} /></Field>
        <Check2 checked={f.hidden} onChange={(v) => setF({ ...f, hidden: v })} label="Hidden from pickers" />
      </Grid>
      {errs.cards && <div className="text-xs text-destructive">{errs.cards}</div>}
      <div className="space-y-1.5">
        {Object.entries(f.cards).map(([c, s]) => {
          const profiles = Object.keys(obj(obj(engines[s.engine]).profiles));
          return (
            <div key={c} className="space-y-0.5" data-testid="layout-card" data-card={c}>
              <div className="grid grid-cols-[2.5rem_1fr_1fr_1fr_1fr] items-center gap-1">
                <span className="font-mono text-xs font-medium">{c}</span>
                <Select value={s.kind} onChange={(v) => setSlot(c, { kind: v as LayoutForm["cards"][string]["kind"] })} options={[["unset", "not in layout"], ["off", "off"], ["same", "same as before"], ["spec", "runs…"]]} />
                {s.kind === "spec" ? (
                  <>
                    <Select value={s.engine} onChange={(v) => setSlot(c, { engine: v, profile: "" })} options={["", ...Object.keys(engines)]} />
                    <Select value={s.profile} onChange={(v) => setSlot(c, { profile: v })} options={["", ...profiles]} />
                    {models.length ? <Select value={s.model} onChange={(v) => setSlot(c, { model: v })} options={["", ...models]} /> : <Text value={s.model} onChange={(v) => setSlot(c, { model: v })} mono placeholder="model" />}
                  </>
                ) : <span className="col-span-3" />}
              </div>
              {errs[`cards.${c}`] && <div className="pl-11 text-[11px] text-destructive">{errs[`cards.${c}`]}</div>}
            </div>
          );
        })}
      </div>
    </FormDialog>
  );
}
