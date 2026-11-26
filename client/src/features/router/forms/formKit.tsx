// Small pieces the router admin forms share (D151): labelled fields, the review-before-save dialog
// (diff, problems, what happens, a reason for the audit) and list rows.
import { useState } from "react";
import { AlertTriangle, Check, Loader2, Pencil, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import type { Errors } from "./formModel";

export const input = "h-8 w-full rounded-md border bg-background px-2 text-sm outline-none focus:border-primary disabled:opacity-60";
export const area = "min-h-[64px] w-full rounded-md border bg-background px-2 py-1.5 font-mono text-xs outline-none focus:border-primary";
export const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function Field({ label, hint, error, children, wide }: { label: string; hint?: string; error?: string; children: React.ReactNode; wide?: boolean }) {
  return (
    <label className={cn("block space-y-1 text-xs", wide && "sm:col-span-2")}>
      <span className="font-medium">{label}</span>
      {children}
      {error ? <span className="block text-destructive" data-testid="field-error">{error}</span> : hint ? <span className="block text-muted-foreground">{hint}</span> : null}
    </label>
  );
}

export function Text({ value, onChange, placeholder, mono, testid, disabled }: { value: string; onChange: (v: string) => void; placeholder?: string; mono?: boolean; testid?: string; disabled?: boolean }) {
  return <input className={cn(input, mono && "font-mono")} value={value} placeholder={placeholder} onChange={(e) => onChange(e.target.value)} data-testid={testid} disabled={disabled} spellCheck={false} />;
}

export function Select({ value, onChange, options, testid }: { value: string; onChange: (v: string) => void; options: (string | [string, string])[]; testid?: string }) {
  return (
    <select className={input} value={value} onChange={(e) => onChange(e.target.value)} data-testid={testid}>
      {options.map((o) => {
        const [v, l] = Array.isArray(o) ? o : [o, o];
        return <option key={v} value={v}>{l}</option>;
      })}
    </select>
  );
}

export function Check2({ checked, onChange, label, testid }: { checked: boolean; onChange: (v: boolean) => void; label: string; testid?: string }) {
  return (
    <label className="flex items-center gap-2 text-xs">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} data-testid={testid} /> {label}
    </label>
  );
}

export function Grid({ children }: { children: React.ReactNode }) {
  return <div className="grid gap-3 sm:grid-cols-2">{children}</div>;
}

export function hasErrors(e: Errors) {
  return Object.keys(e).length > 0;
}

/** A section header with an Add button. */
export function SectionHead({ title, blurb, onAdd, addLabel, testid }: { title: string; blurb: string; onAdd?: () => void; addLabel?: string; testid?: string }) {
  return (
    <div className="flex items-start justify-between gap-3">
      <div>
        <h3 className="text-sm font-semibold">{title}</h3>
        <p className="text-xs text-muted-foreground">{blurb}</p>
      </div>
      {onAdd && (
        <Button size="sm" variant="outline" onClick={onAdd} data-testid={testid}>
          <Plus /> {addLabel ?? "Add"}
        </Button>
      )}
    </div>
  );
}

export function RowActions({ onEdit, onRemove, testid }: { onEdit: () => void; onRemove?: () => void; testid?: string }) {
  return (
    <span className="flex shrink-0 gap-1">
      <Button size="sm" variant="ghost" onClick={onEdit} data-testid={testid ? `${testid}-edit` : undefined} aria-label="Edit"><Pencil /></Button>
      {onRemove && <Button size="sm" variant="ghost" onClick={onRemove} data-testid={testid ? `${testid}-remove` : undefined} aria-label="Remove"><Trash2 /></Button>}
    </span>
  );
}

/** The form dialog frame: title, body, error line, Cancel / Review. */
export function FormDialog({ title, description, onClose, onSubmit, busy, error, children, testid, submitLabel = "Review changes" }: {
  title: string; description: string; onClose: () => void; onSubmit: () => void; busy?: boolean; error?: string | null; children: React.ReactNode; testid?: string; submitLabel?: string;
}) {
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] max-w-2xl overflow-y-auto" data-testid={testid}>
        <DialogTitle>{title}</DialogTitle>
        <DialogDescription>{description}</DialogDescription>
        <div className="space-y-3 pt-2">{children}</div>
        {error && <div className="flex items-start gap-1.5 pt-2 text-xs text-destructive" data-testid="form-error"><AlertTriangle className="mt-0.5 size-3.5 shrink-0" />{error}</div>}
        <div className="flex justify-end gap-2 pt-3">
          <Button size="sm" variant="ghost" onClick={onClose}>Cancel</Button>
          <Button size="sm" onClick={onSubmit} disabled={busy} data-testid="form-submit">{busy && <Loader2 className="animate-spin" />} {submitLabel}</Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

export interface Review {
  ok: boolean;
  diff: string;
  problems: { path: string; message: string; severity: string; line?: number }[];
  consequences: string[];
}

/** Review before save: the diff, problems (an error blocks the save), consequences and a reason. */
export function ReviewDialog({ file, review, onClose, onSave, busy, error }: {
  file: string; review: Review; onClose: () => void; onSave: (reason: string) => void; busy?: boolean; error?: string | null;
}) {
  const [reason, setReason] = useState("");
  const errors = review.problems.filter((p) => p.severity === "error");
  const warnings = review.problems.filter((p) => p.severity !== "error");
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="max-h-[90vh] max-w-3xl overflow-y-auto" data-testid="review-dialog">
        <DialogTitle>Save {file}?</DialogTitle>
        <DialogDescription>
          The router checks the new file with its own loader. Saving backs the old one up, writes the new one atomically, records who and why, and reloads it. Nothing switches a GPU layout: a changed active layout shows as drift with a plan.
        </DialogDescription>
        {errors.length > 0 && (
          <div className="space-y-1 rounded-md border border-destructive/50 p-2 text-xs" data-testid="review-errors">
            <div className="font-medium text-destructive">Not saved: the router would refuse this file.</div>
            {errors.map((p, i) => <div key={i}><span className="font-mono">{p.path || "file"}</span>{p.line ? ` (line ${p.line})` : ""}: {p.message}</div>)}
          </div>
        )}
        {warnings.length > 0 && (
          <div className="space-y-1 rounded-md border border-warning/50 p-2 text-xs">
            {warnings.map((p, i) => <div key={i}><span className="font-mono">{p.path}</span>: {p.message}</div>)}
          </div>
        )}
        {review.consequences.length > 0 && (
          <ul className="list-disc space-y-0.5 pl-5 text-xs text-muted-foreground" data-testid="review-consequences">
            {review.consequences.map((c, i) => <li key={i}>{c}</li>)}
          </ul>
        )}
        <pre className="max-h-72 overflow-auto rounded bg-muted p-2 font-mono text-[11px]" data-testid="review-diff">
          {review.diff.trim() ? review.diff.split("\n").map((l, i) => (
            <div key={i} className={cn(l.startsWith("+") && "text-success", l.startsWith("-") && "text-destructive")}>{l || " "}</div>
          )) : <span className="text-muted-foreground">No change.</span>}
        </pre>
        <Field label="Why (for the audit log)">
          <Text value={reason} onChange={setReason} placeholder="e.g. new card B" testid="review-reason" />
        </Field>
        {error && <div className="text-xs text-destructive" data-testid="review-error">{error}</div>}
        <div className="flex justify-end gap-2">
          <Button size="sm" variant="ghost" onClick={onClose}>Back</Button>
          <Button size="sm" onClick={() => onSave(reason)} disabled={busy || errors.length > 0 || !review.diff.trim()} data-testid="review-save">
            {busy ? <Loader2 className="animate-spin" /> : <Check />} Save
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
