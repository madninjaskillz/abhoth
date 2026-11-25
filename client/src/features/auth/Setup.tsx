import { useState, type FormEvent } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { brandName } from "@/lib/brand";
import { setupAdmin, type AuthState } from "@/lib/auth";
import { MIN_PASSWORD, passwordProblem, usernameProblem } from "./authHelpers";
import { AuthPage } from "./SignIn";
import { Field, errorText, input } from "./parts";

/** First run (no users yet): create the admin account. */
export function Setup({ state, onDone }: { state: AuthState; onDone: () => void }) {
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(state.error ?? null);
  const [touched, setTouched] = useState(false);

  const userProblem = usernameProblem(user);
  const pwProblem = passwordProblem(password, confirm);
  const codeMissing = state.setupCodeNeeded && !code.trim();
  const ready = !userProblem && !pwProblem && !codeMissing;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setTouched(true);
    if (!ready) return;
    setBusy(true);
    setError(null);
    try {
      await setupAdmin(user.trim(), password, state.setupCodeNeeded ? code.trim() : undefined);
      onDone();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <AuthPage subtitle="Create the admin account" wide>
      <form onSubmit={(e) => void submit(e)} className="space-y-3" data-testid="setup-form">
        <p className="text-xs text-muted-foreground">
          This is the first start of this {brandName()} server. The admin manages users and the server's settings; you can add more
          users afterwards.
        </p>
        {state.setupCodeNeeded && (
          <Field label="Setup code" hint={<>Printed in the server's log at startup (<code>First-run setup code: …</code>).</>}>
            <input
              className={`${input} font-mono`}
              autoComplete="off"
              spellCheck={false}
              value={code}
              onChange={(e) => setCode(e.target.value)}
              aria-label="Setup code"
              data-testid="setup-code"
            />
          </Field>
        )}
        <Field label="Username" hint={touched && userProblem ? <span className="text-destructive">{userProblem}</span> : undefined}>
          <input className={input} autoComplete="username" value={user} onChange={(e) => setUser(e.target.value)} autoFocus aria-label="Username" data-testid="setup-username" />
        </Field>
        <Field label="Password" hint={`At least ${MIN_PASSWORD} characters. A long phrase is easiest.`}>
          <input
            className={input}
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            aria-label="Password"
            data-testid="setup-password"
          />
        </Field>
        <Field label="Password again" hint={(touched || confirm) && pwProblem ? <span className="text-destructive">{pwProblem}</span> : undefined}>
          <input
            className={input}
            type="password"
            autoComplete="new-password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            aria-label="Password again"
            data-testid="setup-confirm"
          />
        </Field>
        <Button type="submit" className="w-full" disabled={busy || (touched && !ready)} data-testid="setup-submit">
          {busy && <Loader2 className="animate-spin" />} Create admin account
        </Button>
        {error && <div className="text-center text-sm text-destructive" role="alert" data-testid="setup-error">{error}</div>}
      </form>
    </AuthPage>
  );
}
