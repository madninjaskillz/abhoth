// router.toml in Monaco: checked against the router (the real validator) as you type, problems as
// inline markers and a list, save with a reason behind a diff, 412 when the file moved underneath.
import { useEffect, useRef, useState } from "react";
import Editor, { type OnMount } from "@monaco-editor/react";
import { useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, Loader2, RotateCcw, Save, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { EDITOR_OPTIONS, monaco } from "@/lib/monaco";
import { cn } from "@/lib/utils";
import { RouterApiError, routerApi } from "./routerApi";
import type { CheckResult, Problem } from "./routerModel";
import { DiffBlock } from "./RouterPanes";

const OWNER = "router-config";

export default function ConfigEditor() {
  const doc = useQuery({ queryKey: ["router-config"], queryFn: routerApi.config, refetchOnWindowFocus: false });
  const [text, setText] = useState<string | null>(null);
  const [base, setBase] = useState<{ rev: string; text: string } | null>(null);
  const [check, setCheck] = useState<CheckResult | null>(null);
  const [checking, setChecking] = useState(false);
  const [saveOpen, setSaveOpen] = useState(false);
  const [notice, setNotice] = useState<{ kind: "ok" | "error"; text: string } | null>(null);
  const editorRef = useRef<Parameters<OnMount>[0] | null>(null);

  useEffect(() => {
    if (doc.data && base === null) {
      setBase({ rev: doc.data.rev, text: doc.data.text });
      setText(doc.data.text);
    }
  }, [doc.data, base]);

  // Check 600 ms after the last keystroke.
  useEffect(() => {
    if (text === null) return;
    let live = true;
    const t = setTimeout(() => {
      setChecking(true);
      routerApi
        .check(text)
        .then((r) => live && setCheck(r))
        .catch((e) => live && setNotice({ kind: "error", text: e instanceof Error ? e.message : String(e) }))
        .finally(() => live && setChecking(false));
    }, 600);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [text]);

  // Problems as markers.
  useEffect(() => {
    const model = editorRef.current?.getModel();
    if (!model) return;
    monaco.editor.setModelMarkers(model, OWNER, (check?.problems ?? []).map((p) => {
      const line = Math.min(Math.max(p.line ?? 1, 1), model.getLineCount());
      return {
        severity: p.severity === "error" ? monaco.MarkerSeverity.Error : monaco.MarkerSeverity.Warning,
        message: `${p.path}: ${p.message}`,
        startLineNumber: line,
        startColumn: 1,
        endLineNumber: line,
        endColumn: model.getLineMaxColumn(line),
      };
    }));
  }, [check]);

  const dirty = base !== null && text !== null && text !== base.text;
  const errors = check?.problems.filter((p) => p.severity === "error") ?? [];
  const warnings = check?.problems.filter((p) => p.severity !== "error") ?? [];

  const reveal = (p: Problem) => {
    const e = editorRef.current;
    if (!e || !p.line) return;
    e.revealLineInCenter(p.line);
    e.setPosition({ lineNumber: p.line, column: 1 });
    e.focus();
  };

  if (doc.isLoading || text === null) return <div className="p-4"><Loader2 className="size-4 animate-spin text-muted-foreground" /></div>;
  if (doc.isError) return <div className="p-4 text-xs text-destructive">{String(doc.error)}</div>;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex flex-wrap items-center gap-2 border-b px-4 py-2 text-xs">
        <span className="font-mono text-muted-foreground" title={doc.data?.path}>router.toml · {base?.rev}</span>
        <span className="flex items-center gap-1" data-testid="config-status">
          {checking ? <><Loader2 className="size-3.5 animate-spin" /> checking…</> :
            errors.length ? <><XCircle className="size-3.5 text-destructive" /> {errors.length} error{errors.length === 1 ? "" : "s"}</> :
            <><Check className="size-3.5 text-success" /> valid</>}
          {warnings.length > 0 && !checking && <span className="ml-1 flex items-center gap-1 text-warning"><AlertTriangle className="size-3.5" /> {warnings.length} warning{warnings.length === 1 ? "" : "s"}</span>}
        </span>
        <div className="ml-auto flex gap-2">
          <Button size="sm" variant="ghost" disabled={!dirty} onClick={() => base && setText(base.text)}><RotateCcw /> Revert</Button>
          <Button size="sm" disabled={!dirty || checking || errors.length > 0} onClick={() => setSaveOpen(true)} data-testid="config-save"><Save /> Save…</Button>
        </div>
      </div>
      {doc.data?.error && <div className="border-b px-4 py-1.5 text-xs text-destructive">The file on disk does not load; the router runs the last good config.</div>}
      {notice && <div className={cn("border-b px-4 py-1.5 text-xs", notice.kind === "ok" ? "text-success" : "text-destructive")} role="status">{notice.text}</div>}
      <div className="grid min-h-0 flex-1 grid-rows-[1fr,auto]">
        <div className="min-h-[240px]">
          <Editor
            value={text}
            language="ini"
            theme="openorca"
            options={{ ...EDITOR_OPTIONS, tabSize: 2 }}
            onMount={(e) => { editorRef.current = e; }}
            onChange={(v) => setText(v ?? "")}
          />
        </div>
        {(check?.problems.length ?? 0) > 0 && (
          <ul className="max-h-40 overflow-y-auto border-t text-xs" data-testid="config-problems">
            {check!.problems.map((p, i) => (
              <li key={i}>
                <button className="flex w-full items-start gap-2 px-4 py-1 text-left hover:bg-accent/50" onClick={() => reveal(p)}>
                  {p.severity === "error" ? <XCircle className="mt-0.5 size-3.5 shrink-0 text-destructive" /> : <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-warning" />}
                  <span className="font-mono text-muted-foreground">{p.line ? `line ${p.line}` : "—"}</span>
                  <span className="font-mono">{p.path}</span>
                  <span>{p.message}</span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      {saveOpen && base && check && (
        <SaveDialog
          check={check}
          onClose={() => setSaveOpen(false)}
          onSave={async (reason) => {
            try {
              await routerApi.save(text, base.rev, reason);
              const fresh = await doc.refetch();
              if (fresh.data) setBase({ rev: fresh.data.rev, text: fresh.data.text });
              setNotice({ kind: "ok", text: "Saved and reloaded." });
              setSaveOpen(false);
            } catch (e) {
              const err = e as RouterApiError;
              if (err.status === 412) setNotice({ kind: "error", text: "Someone changed router.toml since you opened it. Copy your edits, reload the page, and merge." });
              else setNotice({ kind: "error", text: err.message });
              setSaveOpen(false);
            }
          }}
        />
      )}
    </div>
  );
}

function SaveDialog({ check, onClose, onSave }: { check: CheckResult; onClose: () => void; onSave: (reason: string) => Promise<void> }) {
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-w-2xl" data-testid="config-save-dialog">
        <DialogTitle>Save router.toml</DialogTitle>
        <DialogDescription>The old file is backed up; who and why go in the audit log.</DialogDescription>
        <DiffBlock diff={check.diff || "(no change)"} />
        {check.consequences.length > 0 && (
          <ul className="list-disc pl-5 text-xs text-muted-foreground">{check.consequences.map((c) => <li key={c}>{c}</li>)}</ul>
        )}
        <input className="h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary" placeholder="Why (optional)" value={reason} onChange={(e) => setReason(e.target.value)} aria-label="Reason" />
        <div className="flex justify-end gap-2">
          <Button size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
          <Button size="sm" disabled={busy} onClick={() => { setBusy(true); void onSave(reason).finally(() => setBusy(false)); }} data-testid="config-save-confirm">
            {busy ? <Loader2 className="animate-spin" /> : <Save />} Save
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
