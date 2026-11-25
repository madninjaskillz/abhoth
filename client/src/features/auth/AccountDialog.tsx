import { useState, type FormEvent } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { KeyRound, Laptop, Loader2, LogOut, Plus, ShieldCheck, ShieldOff, Trash2, UserRound } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Badge, Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/misc";
import { accountApi, authState, type ApiKey, type NewApiKey, type TotpSetup, type WebSessionInfo } from "@/lib/auth";
import { MIN_PASSWORD, ago, when, describeAgent, passwordProblem } from "./authHelpers";
import { CodeForm, CopyButton, RecoveryCodes, TotpQr, errorText } from "./parts";

const small = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";

/** The signed-in user's account: password, two-factor sign-in, API keys and sessions. */
export function AccountDialog({ onClose }: { onClose: () => void }) {
  const account = useQuery({ queryKey: ["account"], queryFn: accountApi.get });
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="flex h-[min(86vh,760px)] max-w-2xl flex-col gap-0 p-0" data-testid="account-dialog">
        <div className="space-y-1 border-b p-4 pb-3">
          <DialogTitle className="flex items-center gap-2">
            <UserRound className="size-4 text-muted-foreground" /> Account
            {account.data && <span className="font-normal text-muted-foreground">· {account.data.username}</span>}
            {account.data?.admin && <Badge>admin</Badge>}
          </DialogTitle>
          <DialogDescription>
            {account.data
              ? <>Created {ago(account.data.created)}{account.data.lastLogin && <> · last sign-in {ago(account.data.lastLogin)}</>}</>
              : "Your password, two-factor sign-in, API keys and signed-in sessions."}
          </DialogDescription>
        </div>
        <Tabs defaultValue="security" className="flex min-h-0 flex-1 flex-col">
          <div className="border-b px-4 py-2">
            <TabsList>
              <TabsTrigger value="security" data-testid="account-tab-security"><ShieldCheck /> Security</TabsTrigger>
              <TabsTrigger value="keys" data-testid="account-tab-keys"><KeyRound /> API keys</TabsTrigger>
              <TabsTrigger value="sessions" data-testid="account-tab-sessions"><Laptop /> Sessions</TabsTrigger>
            </TabsList>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-4">
            {account.isError && <div className="text-sm text-destructive">{errorText(account.error)}</div>}
            <TabsContent value="security" className="space-y-6">
              <PasswordSection />
              {account.data && <TotpSection totp={account.data.totp} left={account.data.recoveryCodesLeft} />}
            </TabsContent>
            <TabsContent value="keys">
              <KeysSection />
            </TabsContent>
            <TabsContent value="sessions">
              <SessionsSection />
            </TabsContent>
          </div>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}

function Section({ title, description, children }: { title: string; description?: React.ReactNode; children: React.ReactNode }) {
  return (
    <section className="space-y-2">
      <h3 className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      {description && <p className="text-xs text-muted-foreground">{description}</p>}
      {children}
    </section>
  );
}

function PasswordSection() {
  const [current, setCurrent] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const problem = passwordProblem(password, confirm);
  const change = useMutation({
    mutationFn: () => accountApi.changePassword(current, password),
    onSuccess: () => {
      setCurrent("");
      setPassword("");
      setConfirm("");
      setMsg({ ok: true, text: "Password changed. Your other sessions were signed out." });
    },
    onError: (e) => setMsg({ ok: false, text: errorText(e) }),
  });
  const submit = (e: FormEvent) => {
    e.preventDefault();
    setMsg(null);
    if (current && !problem) change.mutate();
  };
  return (
    <Section title="Password" description={`At least ${MIN_PASSWORD} characters. Changing it signs out your other sessions.`}>
      <form onSubmit={submit} className="grid gap-2 sm:grid-cols-3" data-testid="password-form">
        <input type="text" autoComplete="username" value={authState().user ?? ""} readOnly hidden />
        <input className={small} type="password" autoComplete="current-password" placeholder="Current password" value={current} onChange={(e) => setCurrent(e.target.value)} aria-label="Current password" />
        <input className={small} type="password" autoComplete="new-password" placeholder="New password" value={password} onChange={(e) => setPassword(e.target.value)} aria-label="New password" />
        <input className={small} type="password" autoComplete="new-password" placeholder="New password again" value={confirm} onChange={(e) => setConfirm(e.target.value)} aria-label="New password again" />
        <div className="flex items-center gap-3 sm:col-span-3">
          <Button type="submit" size="sm" disabled={change.isPending || !current || !!problem} data-testid="password-submit">
            {change.isPending && <Loader2 className="animate-spin" />} Change password
          </Button>
          {password && problem && <span className="text-xs text-muted-foreground">{problem}</span>}
          {msg && <span className={msg.ok ? "text-xs text-success" : "text-xs text-destructive"} role="status">{msg.text}</span>}
        </div>
      </form>
    </Section>
  );
}

type TotpStep = { kind: "idle" } | { kind: "scan"; info: TotpSetup } | { kind: "codes"; codes: string[] } | { kind: "password"; action: "disable" | "codes" };

function TotpSection({ totp, left }: { totp: boolean; left: number }) {
  const qc = useQueryClient();
  const [step, setStep] = useState<TotpStep>({ kind: "idle" });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [password, setPassword] = useState("");
  const refresh = () => void qc.invalidateQueries({ queryKey: ["account"] });

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const start = () => run(async () => setStep({ kind: "scan", info: await accountApi.totpStart() }));
  const confirmCode = (code: string) =>
    run(async () => {
      const r = await accountApi.totpConfirm(code);
      setStep({ kind: "codes", codes: r.recoveryCodes });
      refresh();
    });
  const withPassword = (action: "disable" | "codes") =>
    run(async () => {
      if (action === "disable") {
        await accountApi.totpDisable(password);
        setStep({ kind: "idle" });
      } else {
        const r = await accountApi.recoveryCodes(password);
        setStep({ kind: "codes", codes: r.recoveryCodes });
      }
      setPassword("");
      refresh();
    });

  return (
    <Section
      title="Two-factor sign-in"
      description="An authenticator app code at each sign-in, on top of the password. Recovery codes stand in for the app if you lose it."
    >
      <div className="rounded-md border p-3" data-testid="totp-section">
        {step.kind === "idle" && (
          <div className="flex flex-wrap items-center gap-3">
            {totp ? (
              <>
                <Badge variant="success"><ShieldCheck className="mr-1 size-3" /> On</Badge>
                <span className="text-xs text-muted-foreground" data-testid="recovery-left">{left} recovery code{left === 1 ? "" : "s"} left</span>
                <span className="ml-auto" />
                <Button size="sm" variant="outline" onClick={() => setStep({ kind: "password", action: "codes" })} data-testid="regen-codes">
                  New recovery codes
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setStep({ kind: "password", action: "disable" })} data-testid="totp-disable">
                  <ShieldOff /> Turn off
                </Button>
              </>
            ) : (
              <>
                <Badge variant="outline">Off</Badge>
                <span className="ml-auto" />
                <Button size="sm" onClick={() => void start()} disabled={busy} data-testid="totp-enable">
                  {busy ? <Loader2 className="animate-spin" /> : <ShieldCheck />} Turn on
                </Button>
              </>
            )}
          </div>
        )}
        {step.kind === "scan" && (
          <div className="mx-auto max-w-xs space-y-3">
            <TotpQr info={step.info} />
            <CodeForm busy={busy} onSubmit={(c) => void confirmCode(c)} submitLabel="Turn on" />
            <Button size="sm" variant="ghost" className="w-full" onClick={() => setStep({ kind: "idle" })}>Cancel</Button>
          </div>
        )}
        {step.kind === "codes" && (
          <div className="mx-auto max-w-sm">
            <RecoveryCodes codes={step.codes} onDone={() => setStep({ kind: "idle" })} />
          </div>
        )}
        {step.kind === "password" && (
          <form
            className="flex flex-wrap items-center gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              if (password) void withPassword(step.action);
            }}
          >
            <span className="w-full text-xs text-muted-foreground">
              {step.action === "disable" ? "Confirm with your password to turn two-factor sign-in off." : "Confirm with your password; the old recovery codes stop working."}
            </span>
            <input
              className={`${small} max-w-xs`}
              type="password"
              autoComplete="current-password"
              placeholder="Password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoFocus
              aria-label="Password"
              data-testid="totp-password"
            />
            <Button type="submit" size="sm" variant={step.action === "disable" ? "destructive" : "default"} disabled={busy || !password} data-testid="totp-password-submit">
              {busy && <Loader2 className="animate-spin" />} {step.action === "disable" ? "Turn off" : "Make new codes"}
            </Button>
            <Button type="button" size="sm" variant="ghost" onClick={() => { setStep({ kind: "idle" }); setPassword(""); }}>Cancel</Button>
          </form>
        )}
        {error && <div className="mt-2 text-xs text-destructive" role="alert">{error}</div>}
      </div>
    </Section>
  );
}

function KeysSection() {
  const qc = useQueryClient();
  const keys = useQuery({ queryKey: ["account-keys"], queryFn: accountApi.keys });
  const [name, setName] = useState("");
  const [created, setCreated] = useState<NewApiKey | null>(null);
  const [revoking, setRevoking] = useState<ApiKey | null>(null);
  const [error, setError] = useState<string | null>(null);
  const create = useMutation({
    mutationFn: () => accountApi.createKey(name.trim()),
    onSuccess: (k) => {
      setCreated(k);
      setName("");
      void qc.invalidateQueries({ queryKey: ["account-keys"] });
    },
    onError: (e) => setError(errorText(e)),
  });
  const revoke = useMutation({
    mutationFn: (id: number) => accountApi.revokeKey(id),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["account-keys"] }),
    onError: (e) => setError(errorText(e)),
  });
  return (
    <Section
      title="API keys"
      description={<>For scripts and the CLI: send <code>Authorization: Bearer &lt;key&gt;</code>. A key acts as you; revoke it when it's no longer needed.</>}
    >
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          setError(null);
          if (name.trim()) create.mutate();
        }}
      >
        <input className={small} placeholder="Name (what it's for)" value={name} maxLength={64} onChange={(e) => setName(e.target.value)} aria-label="Key name" data-testid="key-name" />
        <Button type="submit" size="sm" disabled={create.isPending || !name.trim()} data-testid="key-create">
          {create.isPending ? <Loader2 className="animate-spin" /> : <Plus />} Create key
        </Button>
      </form>
      {created && (
        <div className="space-y-2 rounded-md border border-primary/40 bg-primary/5 p-3" data-testid="api-key-created">
          <div className="text-xs font-medium">Key "{created.name}" created. Copy it now: it is not shown again.</div>
          <div className="break-all rounded border bg-background p-2 font-mono text-xs" data-testid="api-key-value">{created.key}</div>
          <div className="flex items-center gap-2">
            <CopyButton text={created.key} testId="api-key-copy" />
            <span className="text-[11px] text-muted-foreground">Use as <code>Authorization: Bearer {created.prefix}…</code></span>
            <span className="ml-auto" />
            <Button size="sm" variant="ghost" onClick={() => setCreated(null)}>Done</Button>
          </div>
        </div>
      )}
      {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
      <div className="rounded-md border">
        {keys.isLoading && <div className="p-3 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /></div>}
        {keys.isError && <div className="p-3 text-xs text-destructive">{errorText(keys.error)}</div>}
        {keys.data?.length === 0 && <div className="p-3 text-xs text-muted-foreground">No API keys yet.</div>}
        {!!keys.data?.length && (
          <table className="w-full text-xs" data-testid="key-table">
            <thead className="text-left text-muted-foreground">
              <tr className="border-b">
                <th className="px-3 py-1.5 font-medium">Name</th>
                <th className="px-2 font-medium">Key</th>
                <th className="px-2 font-medium">Created</th>
                <th className="px-2 font-medium">Last used</th>
                <th className="px-2" />
              </tr>
            </thead>
            <tbody>
              {keys.data.map((k) => (
                <tr key={k.id} className="border-b last:border-0" data-testid="key-row">
                  <td className="px-3 py-1.5">{k.name}</td>
                  <td className="px-2 font-mono text-muted-foreground">{k.prefix}…</td>
                  <td className="px-2 text-muted-foreground" title={when(k.created)}>{ago(k.created)}</td>
                  <td className="px-2 text-muted-foreground" title={when(k.lastUsed)}>{k.lastUsed ? ago(k.lastUsed) : "never"}</td>
                  <td className="px-2 text-right">
                    <Button size="icon-sm" variant="ghost" onClick={() => setRevoking(k)} aria-label={`Revoke ${k.name}`} data-testid="key-revoke">
                      <Trash2 />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <ConfirmDialog
        open={revoking !== null}
        title={`Revoke "${revoking?.name}"?`}
        body="Scripts using this key stop working at once. This can't be undone."
        confirm="Revoke"
        destructive
        onConfirm={() => {
          if (revoking) revoke.mutate(revoking.id);
          setRevoking(null);
        }}
        onCancel={() => setRevoking(null)}
      />
    </Section>
  );
}

function SessionsSection() {
  const qc = useQueryClient();
  const sessions = useQuery({ queryKey: ["account-sessions"], queryFn: accountApi.sessions });
  const [everywhere, setEverywhere] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const revoke = useMutation({
    mutationFn: (s: WebSessionInfo) => accountApi.revokeSession(s.id).then(() => s),
    onSuccess: (s) => {
      if (s.current) location.reload();
      else void qc.invalidateQueries({ queryKey: ["account-sessions"] });
    },
    onError: (e) => setError(errorText(e)),
  });
  const all = useMutation({
    mutationFn: accountApi.revokeAll,
    onSuccess: () => location.assign("/"),
    onError: (e) => setError(errorText(e)),
  });
  return (
    <Section title="Signed-in sessions" description="Browsers and windows signed in as you. Sign out any you don't recognise.">
      {error && <div className="text-xs text-destructive" role="alert">{error}</div>}
      <div className="divide-y rounded-md border" data-testid="session-list">
        {sessions.isLoading && <div className="p-3 text-xs text-muted-foreground"><Loader2 className="size-3.5 animate-spin" /></div>}
        {sessions.isError && <div className="p-3 text-xs text-destructive">{errorText(sessions.error)}</div>}
        {sessions.data?.map((s) => (
          <div key={s.id} className="flex items-center gap-3 px-3 py-2 text-xs" data-testid="session-row">
            <Laptop className="size-4 shrink-0 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className="truncate font-medium">{s.desktop ? "Desktop app" : describeAgent(s.userAgent) || "Unknown browser"}</span>
                {s.current && <Badge variant="success">this session</Badge>}
              </div>
              <div className="truncate text-muted-foreground">
                {s.ip ?? "unknown address"} · signed in {ago(s.created)} · active {ago(s.lastSeen)}
              </div>
            </div>
            <Button size="sm" variant="ghost" onClick={() => revoke.mutate(s)} disabled={revoke.isPending} data-testid="session-revoke">
              <LogOut /> Sign out
            </Button>
          </div>
        ))}
      </div>
      <Button size="sm" variant="outline" onClick={() => setEverywhere(true)} data-testid="sign-out-everywhere">
        <LogOut /> Sign out everywhere
      </Button>
      <ConfirmDialog
        open={everywhere}
        title="Sign out everywhere?"
        body="Every session of yours ends, this one included. API keys keep working."
        confirm="Sign out everywhere"
        destructive
        onConfirm={() => {
          setEverywhere(false);
          all.mutate();
        }}
        onCancel={() => setEverywhere(false)}
      />
    </Section>
  );
}

