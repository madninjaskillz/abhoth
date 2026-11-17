import { useCallback, useEffect, useRef, useState } from "react";
import { FileText, Loader2, X } from "lucide-react";
import { uploadAttachments } from "@/lib/api";
import { cn } from "@/lib/utils";

export interface PendingAttachment {
  key: string;
  name: string;
  /** Project-relative path once uploaded. */
  path?: string;
  kind: "image" | "file";
  /** Object URL for an image thumbnail. */
  preview?: string;
  uploading: boolean;
  error?: string;
}

const isImage = (f: File) => /^image\/(png|jpeg|gif|webp)$/.test(f.type);
let counter = 0;

/** The prompt's attachments: added by paste or drop, uploaded straight away, removable until sent. */
export function useAttachments(pid: string) {
  const [items, setItems] = useState<PendingAttachment[]>([]);
  const itemsRef = useRef(items);
  itemsRef.current = items;

  useEffect(() => () => itemsRef.current.forEach((a) => a.preview && URL.revokeObjectURL(a.preview)), []);

  const add = useCallback(
    (files: File[]) => {
      if (!files.length) return;
      const added = files.map<PendingAttachment>((f) => ({
        key: `a${++counter}`,
        name: f.name || (isImage(f) ? "pasted.png" : "file"),
        kind: isImage(f) ? "image" : "file",
        preview: isImage(f) ? URL.createObjectURL(f) : undefined,
        uploading: true,
      }));
      setItems((cur) => [...cur, ...added]);
      // One upload per file, so one bad file doesn't sink the others.
      files.forEach((f, i) => {
        const key = added[i].key;
        const named = f.name ? f : new File([f], added[i].name, { type: f.type });
        uploadAttachments(pid, [named])
          .then(([saved]) =>
            setItems((cur) => cur.map((a) => (a.key === key ? { ...a, uploading: false, path: saved.path, kind: saved.kind } : a))),
          )
          .catch((e: unknown) =>
            setItems((cur) => cur.map((a) => (a.key === key ? { ...a, uploading: false, error: e instanceof Error ? e.message : String(e) } : a))),
          );
      });
    },
    [pid],
  );

  const remove = (key: string) =>
    setItems((cur) => {
      const gone = cur.find((a) => a.key === key);
      if (gone?.preview) URL.revokeObjectURL(gone.preview);
      return cur.filter((a) => a.key !== key);
    });

  const clear = () =>
    setItems((cur) => {
      cur.forEach((a) => a.preview && URL.revokeObjectURL(a.preview));
      return [];
    });

  const ready = items.filter((a) => a.path && !a.error).map((a) => a.path!);
  const pending = items.some((a) => a.uploading);
  return { items, add, remove, clear, ready, pending };
}

export function AttachmentChips({ items, onRemove }: { items: PendingAttachment[]; onRemove: (key: string) => void }) {
  if (!items.length) return null;
  return (
    <div className="flex flex-wrap gap-1.5 px-1 pb-1.5 pt-0.5" data-testid="attachments">
      {items.map((a) => (
        <div
          key={a.key}
          className={cn(
            "group relative flex h-12 max-w-56 items-center gap-1.5 rounded-md border bg-panel pr-6 text-xs",
            a.error && "border-destructive/50",
          )}
          title={a.error ?? a.path ?? a.name}
          data-testid="attachment-chip"
        >
          {a.preview ? (
            <img src={a.preview} alt={a.name} className="h-full w-12 shrink-0 rounded-l-md object-cover" />
          ) : (
            <FileText className="ml-2 size-5 shrink-0 text-muted-foreground" />
          )}
          <div className="min-w-0">
            <div className="truncate font-mono">{a.name}</div>
            <div className={cn("truncate text-[10px]", a.error ? "text-destructive" : "text-muted-foreground")}>
              {a.uploading ? (
                <span className="inline-flex items-center gap-1"><Loader2 className="size-3 animate-spin" /> uploading</span>
              ) : (
                a.error ?? a.kind
              )}
            </div>
          </div>
          <button
            className="absolute right-1 top-1 rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={() => onRemove(a.key)}
            aria-label={`Remove ${a.name}`}
            data-testid="attachment-remove"
          >
            <X className="size-3" />
          </button>
        </div>
      ))}
    </div>
  );
}
