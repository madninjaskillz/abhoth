import { useEffect, useState, type FormEvent } from "react";
import { brandName } from "@/lib/brand";
import { Loader2 } from "lucide-react";
import { Mascot } from "@/components/Mascot";
import { Button } from "@/components/ui/button";
import { enrollInfo, signIn, submitCode, type AuthState, type TotpSetup } from "@/lib/auth";
import { CodeForm, RecoveryCodes, TotpQr, errorText, input } from "./parts";

/** The sign-in page: password, then a 2FA code (or a recovery code), or 2FA setup when the server requires it. */
export function SignIn({ state, onDone }: { state: AuthState; onDone: () => void }) {
  const [stage, setStage] = useState(state.stage);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [error, setError] = useState<string | null>(state.error ?? null);
  const [busy, setBusy] = useState(false);

  const run = async (fn: () => Promise<AuthState>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await fn();
      if (s.recoveryCodes?.length) setCodes(s.recoveryCodes);
      else if (s.stage === "ok") onDone();
      else setStage(s.stage);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const subtitle = codes
    ? "Save your recovery codes"
    : stage === "enroll"
      ? "Set up two-factor sign-in"
      : stage === "totp"
        ? "Enter the code from your authenticator app"
        : "Sign in to continue";

  return (
    <AuthPage subtitle={subtitle}>
      {codes ? (
        <RecoveryCodes codes={codes} onDone={onDone} doneLabel="Continue" />
      ) : (
        <>
          {stage === "signedOut" && <PasswordForm busy={busy} onSubmit={(u, p) => run(() => signIn(u, p))} />}
          {stage === "totp" && <CodeForm busy={busy} allowRecovery onSubmit={(c) => run(() => submitCode(c))} />}
          {stage === "enroll" && <Enroll busy={busy} onSubmit={(c) => run(() => submitCode(c))} />}
        </>
      )}
      {error && <div className="mt-3 text-center text-sm text-destructive" role="alert" data-testid="signin-error">{error}</div>}
    </AuthPage>
  );
}

/** The centred card with the mascot, shared by sign-in and setup. */
export function AuthPage({ subtitle, children, wide }: { subtitle: string; children: React.ReactNode; wide?: boolean }) {
  return (
    <div className="flex min-h-full items-center justify-center px-4 py-10">
      <div className={wide ? "w-full max-w-md" : "w-full max-w-sm"}>
        <Mascot size={128} className="mb-6" />
        <h1 className="mb-1 text-center text-lg font-semibold text-primary">{brandName()}</h1>
        <p className="mb-6 text-center text-sm text-muted-foreground">{subtitle}</p>
        {children}
      </div>
    </div>
  );
}

function PasswordForm({ busy, onSubmit }: { busy: boolean; onSubmit: (u: string, p: string) => void }) {
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (user && password) onSubmit(user, password);
  };
  return (
    <form onSubmit={submit} className="space-y-3" data-testid="signin-form">
      <input className={input} autoComplete="username" placeholder="Username" value={user} onChange={(e) => setUser(e.target.value)} autoFocus aria-label="Username" />
      <input className={input} type="password" autoComplete="current-password" placeholder="Password" value={password} onChange={(e) => setPassword(e.target.value)} aria-label="Password" />
      <Button type="submit" className="w-full" disabled={busy || !user || !password}>
        {busy && <Loader2 className="animate-spin" />} Sign in
      </Button>
    </form>
  );
}

function Enroll({ busy, onSubmit }: { busy: boolean; onSubmit: (c: string) => void }) {
  const [info, setInfo] = useState<TotpSetup | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    enrollInfo().then(setInfo, (e) => setError(errorText(e)));
  }, []);
  return (
    <div className="space-y-3">
      <p className="text-center text-xs text-muted-foreground">This server requires two-factor sign-in.</p>
      {info ? <TotpQr info={info} /> : <div className="flex justify-center p-8">{error ?? <Loader2 className="animate-spin" />}</div>}
      <CodeForm busy={busy} onSubmit={onSubmit} submitLabel="Turn on" />
    </div>
  );
}
