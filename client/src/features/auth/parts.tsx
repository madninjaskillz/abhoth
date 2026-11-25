import { useState, type FormEvent, type ReactNode } from "react";
import { Check, Copy, Download, KeyRound, Loader2, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { brandName } from "@/lib/brand";
import type { TotpSetup } from "@/lib/auth";
import { codeInput, codesText } from "./authHelpers";

export const input =
  "h-9 w-full rounded-md border bg-background px-3 text-sm outline-none focus:ring-2 focus:ring-ring disabled:opacity-60";

export const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Copy to the clipboard; true when it worked. */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

export function CopyButton({ text, label = "Copy", testId }: { text: string; label?: string; testId?: string }) {
  const [done, setDone] = useState(false);
  return (
    <Button
      type="button"
      size="sm"
      variant="outline"
      onClick={() => void copyText(text).then((ok) => { setDone(ok); if (ok) setTimeout(() => setDone(false), 1500); })}
      data-testid={testId}
    >
      {done ? <Check /> : <Copy />} {done ? "Copied" : label}
    </Button>
  );
}

/** A 6-digit code field, or (toggle) a recovery code. */
export function CodeForm({
  busy,
  onSubmit,
  allowRecovery,
  submitLabel = "Verify",
}: {
  busy: boolean;
  onSubmit: (c: string) => void;
  allowRecovery?: boolean;
  submitLabel?: string;
}) {
  const [raw, setRaw] = useState("");
  const [recovery, setRecovery] = useState(false);
  const code = codeInput(raw, recovery);
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (code.complete) onSubmit(code.value);
  };
  return (
    <form onSubmit={submit} className="space-y-3" data-testid="code-form">
      <input
        className={`${input} text-center font-mono ${recovery ? "tracking-[0.2em]" : "tracking-[0.4em]"}`}
        inputMode={recovery ? "text" : "numeric"}
        autoComplete="one-time-code"
        autoCapitalize="off"
        spellCheck={false}
        placeholder={recovery ? "abcde-12345" : "123456"}
        value={code.value}
        onChange={(e) => setRaw(e.target.value)}
        autoFocus
        aria-label={recovery ? "Recovery code" : "Authentication code"}
        data-testid="code-input"
      />
      <Button type="submit" className="w-full" disabled={busy || !code.complete}>
        {busy && <Loader2 className="animate-spin" />} {submitLabel}
      </Button>
      {allowRecovery && (
        <button
          type="button"
          className="block w-full text-center text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
          onClick={() => {
            setRecovery((r) => !r);
            setRaw("");
          }}
          data-testid="use-recovery"
        >
          {recovery ? "Use the authenticator app" : "Use a recovery code"}
        </button>
      )}
    </form>
  );
}

/** The QR code and the secret as text, for an authenticator app. */
export function TotpQr({ info }: { info: TotpSetup }) {
  return (
    <div className="space-y-2" data-testid="totp-qr">
      <p className="text-xs text-muted-foreground">
        <ShieldCheck className="mr-1 inline size-3.5 text-primary" />
        Scan this with an authenticator app (1Password, Google Authenticator, Aegis…), then type the 6-digit code it shows.
      </p>
      <img src={info.qr} alt="2FA QR code" className="mx-auto size-48 rounded-md bg-white p-1" data-testid="enroll-qr" />
      <div className="flex items-center justify-center gap-2">
        <span className="break-all text-center font-mono text-[11px] text-muted-foreground" data-testid="enroll-secret">{info.secret}</span>
      </div>
    </div>
  );
}

/** Recovery codes, shown once: copy or download, then confirm they're saved. */
export function RecoveryCodes({ codes, onDone, doneLabel = "I saved them" }: { codes: string[]; onDone: () => void; doneLabel?: string }) {
  const [saved, setSaved] = useState(false);
  const text = codesText(codes, brandName());
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `${brandName().toLowerCase()}-recovery-codes.txt`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  return (
    <div className="space-y-3" data-testid="recovery-codes">
      <p className="text-xs text-muted-foreground">
        <KeyRound className="mr-1 inline size-3.5 text-primary" />
        Recovery codes let you sign in if you lose the authenticator. Each works once. Keep them somewhere safe: they are not shown again.
      </p>
      <ol className="grid grid-cols-2 gap-x-4 gap-y-1 rounded-md border bg-background p-3 font-mono text-sm">
        {codes.map((c) => (
          <li key={c} className="text-center">{c}</li>
        ))}
      </ol>
      <div className="flex flex-wrap justify-center gap-2">
        <CopyButton text={text} testId="copy-recovery" />
        <Button type="button" size="sm" variant="outline" onClick={download}>
          <Download /> Download
        </Button>
      </div>
      <label className="flex items-center justify-center gap-2 text-xs text-muted-foreground">
        <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} data-testid="recovery-saved" />
        I have saved these codes
      </label>
      <Button type="button" className="w-full" disabled={!saved} onClick={onDone} data-testid="recovery-done">
        {doneLabel}
      </Button>
    </div>
  );
}

/** A labelled field row. */
export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <label className="block space-y-1">
      <span className="text-xs font-medium text-muted-foreground">{label}</span>
      {children}
      {hint && <span className="block text-[11px] text-muted-foreground">{hint}</span>}
    </label>
  );
}
