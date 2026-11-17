import { useQuery } from "@tanstack/react-query";
import { DiffEditor, loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";
import "monaco-editor/esm/vs/basic-languages/monaco.contribution";
import EditorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";
import { Loader2 } from "lucide-react";
import { api } from "@/lib/api";
import { languageOf } from "@/lib/utils";

// Monaco from the bundle (never a CDN): the UI must work offline and on a LAN.
(self as unknown as { MonacoEnvironment: unknown }).MonacoEnvironment = { getWorker: () => new EditorWorker() };
loader.config({ monaco });

monaco.editor.defineTheme("openorca", {
  base: "vs-dark",
  inherit: true,
  rules: [],
  colors: {
    "editor.background": "#0d1015",
    "diffEditor.insertedTextBackground": "#2ea04333",
    "diffEditor.removedTextBackground": "#f8514933",
    "diffEditor.insertedLineBackground": "#2ea0431a",
    "diffEditor.removedLineBackground": "#f851491a",
  },
});

export function DiffView({ path, staged, origPath }: { path: string; staged: boolean; origPath?: string | null }) {
  const diff = useQuery({
    queryKey: ["git", "diff", path, staged, origPath ?? ""],
    queryFn: () => api.gitDiff(path, staged, origPath),
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
            options={{
              readOnly: true,
              renderSideBySide: true,
              useInlineViewWhenSpaceIsLimited: true,
              minimap: { enabled: false },
              fontSize: 12,
              fontFamily: '"JetBrains Mono", "Cascadia Code", Consolas, monospace',
              scrollBeyondLastLine: false,
              automaticLayout: true,
              renderOverviewRuler: false,
              hideUnchangedRegions: { enabled: true },
            }}
          />
        )}
      </div>
    </div>
  );
}
