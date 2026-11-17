import { useQuery } from "@tanstack/react-query";
import { DiffEditor } from "@monaco-editor/react";
import { Loader2 } from "lucide-react";
import { projectApi } from "@/lib/api";
import { DIFF_OPTIONS } from "@/lib/monaco";
import { languageOf } from "@/lib/utils";

export function DiffView({ pid, wt, path, staged, origPath }: { pid: string; wt: string; path: string; staged: boolean; origPath?: string | null }) {
  const diff = useQuery({
    queryKey: ["git", pid, wt, "diff", path, staged, origPath ?? ""],
    queryFn: () => projectApi(pid, wt).gitDiff(path, staged, origPath),
  });

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-7 shrink-0 items-center gap-2 border-b px-3 text-[11px] text-muted-foreground">
        <span className="truncate font-mono text-foreground">{path}</span>
        <span>{staged ? "HEAD ↔ index" : "index ↔ working tree"}</span>
      </div>
      <div className="min-h-0 flex-1" data-testid="diff-view">
        {diff.isLoading && (
          <div className="flex h-full items-center justify-center text-muted-foreground">
            <Loader2 className="size-4 animate-spin" />
          </div>
        )}
        {diff.error && <div className="p-3 text-xs text-destructive">{String(diff.error)}</div>}
        {diff.data && (diff.data.binary || diff.data.tooLarge) && (
          <div className="p-3 text-xs text-muted-foreground">{diff.data.binary ? "Binary file." : "File too large to diff here."}</div>
        )}
        {diff.data && !diff.data.binary && !diff.data.tooLarge && (
          <DiffEditor
            original={diff.data.original}
            modified={diff.data.modified}
            language={languageOf(path)}
            theme="openorca"
            options={{ ...DIFF_OPTIONS, readOnly: true }}
          />
        )}
      </div>
    </div>
  );
}
