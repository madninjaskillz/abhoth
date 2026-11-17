import { useCallback, useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import Editor, { DiffEditor, type OnMount } from "@monaco-editor/react";
import { FileDiff, Loader2, MessagesSquare, Save, X } from "lucide-react";
import { ApiError, projectApi } from "@/lib/api";
import { DIFF_OPTIONS, EDITOR_OPTIONS, monaco } from "@/lib/monaco";
import { useProject, useSession } from "@/store/session";
import { useWorkspace, type EditorTab } from "@/store/workspace";
import { ChatPanel } from "@/features/chat/ChatPanel";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Tip } from "@/components/ui/misc";
import { cn, languageOf } from "@/lib/utils";

const modelUri = (pid: string, tab: EditorTab) =>
  monaco.Uri.from({ scheme: "openorca", authority: pid, path: "/" + encodeURIComponent(tab.wt) + "/" + tab.path });

/** What the editor knows about a file on disk (per open tab). */
interface Saved {
  etag: string | null;
  bom: boolean;
  version: number;
}

const savedState = new Map<string, Saved>();

/** The chat and the open files, as tabs across the centre of the workspace. */
export function EditorArea() {
  const { id: pid } = useProject();
  const ui = useWorkspace((s) => s.projects[pid]);
  const patch = useWorkspace((s) => s.patch);
  const closeTab = useWorkspace((s) => s.closeTab);
  const busy = useSession((s) => s.busy);
  const [closing, setClosing] = useState<EditorTab | null>(null);
  if (!ui) return null;

  const close = (tab: EditorTab) => {
    if (tab.dirty) setClosing(tab);
    else doClose(tab);
  };
  const doClose = (tab: EditorTab) => {
    monaco.editor.getModel(modelUri(pid, tab))?.dispose();
    savedState.delete(pid + tab.key);
    closeTab(pid, tab.key);
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-8 shrink-0 items-end gap-px overflow-x-auto border-b bg-panel px-1" role="tablist" data-testid="center-tabs">
        <TabButton active={ui.center === "chat"} onClick={() => patch(pid, { center: "chat" })} testId="tab-chat">
          <MessagesSquare className="size-3.5" /> Chat
          {busy && <span className="size-1.5 animate-pulse rounded-full bg-primary" />}
        </TabButton>
        {ui.tabs.map((t) => (
          <TabButton key={t.key} active={ui.center === t.key} onClick={() => patch(pid, { center: t.key })} title={t.path} testId="tab-file">
            <span className={cn("max-w-40 truncate", t.diff && "italic")}>{t.name}</span>
            {t.dirty ? (
              <span className="size-2 rounded-full bg-sky-300" title="Unsaved" />
            ) : null}
            <span
              role="button"
              aria-label={`Close ${t.name}`}
              className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
              onClick={(e) => {
                e.stopPropagation();
                close(t);
              }}
            >
              <X className="size-3" />
            </span>
          </TabButton>
        ))}
      </div>
      <div className="relative min-h-0 flex-1">
        <div className={cn("absolute inset-0", ui.center !== "chat" && "invisible")}>
          <ChatPanel />
        </div>
        {ui.tabs.map((t) => (
          <div key={t.key} className={cn("absolute inset-0", ui.center !== t.key && "invisible")}>
            <FileEditor tab={t} visible={ui.center === t.key} />
          </div>
        ))}
      </div>
      <ConfirmDialog
        open={closing !== null}
        title="Close without saving?"
        body={<>Your changes to <span className="break-all font-mono text-foreground">{closing?.path}</span> will be lost.</>}
        confirm="Discard changes"
        destructive
        onConfirm={() => {
          if (closing) doClose(closing);
          setClosing(null);
        }}
        onCancel={() => setClosing(null)}
      />
    </div>
  );
}

function TabButton({ active, onClick, children, title, testId }: { active: boolean; onClick: () => void; children: React.ReactNode; title?: string; testId?: string }) {
  return (
    <button
      role="tab"
      aria-selected={active}
      onClick={onClick}
      title={title}
      data-testid={testId}
      className={cn(
        "flex h-7 shrink-0 items-center gap-1.5 rounded-t-md border border-b-0 px-2.5 text-xs",
        active ? "border-border bg-background text-foreground" : "border-transparent text-muted-foreground hover:bg-accent/50",
      )}
    >
      {children}
    </button>
  );
}

/** One open file: Monaco with save (Ctrl+S), or its diff against HEAD (read-only, of the current buffer). */
function FileEditor({ tab, visible }: { tab: EditorTab; visible: boolean }) {
  const { id: pid } = useProject();
  const qc = useQueryClient();
  const setDirty = useWorkspace((s) => s.setDirty);
  const setDiff = useWorkspace((s) => s.setDiff);
  const api = projectApi(pid, tab.wt);
  const stateKey = pid + tab.key;
  const editorRef = useRef<monaco.editor.IStandaloneCodeEditor | null>(null);
  const [status, setStatus] = useState<{ kind: "info" | "error" | "conflict" | "external"; text: string } | null>(null);
  const [saving, setSaving] = useState(false);

  const file = useQuery({
    queryKey: ["file", pid, tab.wt, tab.path],
    queryFn: () => api.readFile(tab.path),
    staleTime: Infinity,
    gcTime: 0,
    retry: false,
  });
  const head = useQuery({
    queryKey: ["head", pid, tab.wt, tab.path],
    queryFn: () => api.head(tab.path),
    enabled: tab.diff,
  });

  // The model holds the buffer for as long as the tab is open (diff view edits the same one).
  const model = useCallback(() => {
    const uri = modelUri(pid, tab);
    let m = monaco.editor.getModel(uri);
    if (!m && file.data && !file.data.binary && !file.data.tooLarge) {
      m = monaco.editor.createModel(file.data.content, languageOf(tab.path), uri);
      savedState.set(stateKey, { etag: file.data.etag, bom: file.data.bom, version: m.getAlternativeVersionId() });
      m.onDidChangeContent(() => {
        const saved = savedState.get(stateKey);
        setDirty(pid, tab.key, !!saved && m!.getAlternativeVersionId() !== saved.version);
      });
    }
    return m;
  }, [pid, tab, file.data, stateKey, setDirty]);

  const save = useCallback(
    async (force = false) => {
      const m = model();
      const saved = savedState.get(stateKey);
      if (!m || !saved) return;
      setSaving(true);
      try {
        const r = await api.writeFile(tab.path, m.getValue(), force ? null : saved.etag, saved.bom);
        savedState.set(stateKey, { ...saved, etag: r.etag ?? null, version: m.getAlternativeVersionId() });
        setDirty(pid, tab.key, false);
        setStatus({ kind: "info", text: "Saved." });
        void qc.invalidateQueries({ queryKey: ["changes", pid] });
        void qc.invalidateQueries({ queryKey: ["git", pid] });
        void qc.invalidateQueries({ queryKey: ["head", pid, tab.wt, tab.path] });
      } catch (e) {
        if (e instanceof ApiError && e.status === 409) setStatus({ kind: "conflict", text: "The file changed on disk since you opened it." });
        else setStatus({ kind: "error", text: e instanceof Error ? e.message : String(e) });
      } finally {
        setSaving(false);
      }
    },
    [api, model, pid, qc, stateKey, setDirty, tab.key, tab.path, tab.wt],
  );

  const reload = useCallback(async () => {
    const fresh = await api.readFile(tab.path);
    const m = model();
    if (m) {
      m.setValue(fresh.content);
      savedState.set(stateKey, { etag: fresh.etag, bom: fresh.bom, version: m.getAlternativeVersionId() });
      setDirty(pid, tab.key, false);
    }
    setStatus(null);
  }, [api, model, pid, stateKey, setDirty, tab.key, tab.path]);

  // Changes on disk (the agent, the terminal): reload a clean buffer, warn about a dirty one.
  useEffect(() => {
    const dir = tab.path.includes("/") ? tab.path.slice(0, tab.path.lastIndexOf("/")) : "";
    const onFiles = async (e: Event) => {
      const d = (e as CustomEvent<{ pid: string; dirs: string[] | null }>).detail;
      if (d.pid !== pid || (d.dirs && !d.dirs.includes(dir))) return;
      const saved = savedState.get(stateKey);
      if (!saved) return;
      try {
        const fresh = await api.readFile(tab.path);
        if (fresh.etag === saved.etag) return;
        const m = model();
        if (!m) return;
        if (m.getAlternativeVersionId() === saved.version) {
          m.setValue(fresh.content);
          savedState.set(stateKey, { etag: fresh.etag, bom: fresh.bom, version: m.getAlternativeVersionId() });
          setDirty(pid, tab.key, false);
          setStatus({ kind: "info", text: "Reloaded: the file changed on disk." });
        } else {
          setStatus({ kind: "external", text: "The file changed on disk; you have unsaved changes." });
        }
      } catch {
        /* deleted, say: leave the buffer alone */
      }
    };
    window.addEventListener("openorca:files", onFiles);
    return () => window.removeEventListener("openorca:files", onFiles);
  }, [api, model, pid, stateKey, setDirty, tab.key, tab.path]);

  useEffect(() => {
    if (visible) editorRef.current?.focus();
  }, [visible]);

  const onMount: OnMount = (editor) => {
    editorRef.current = editor;
    const m = model();
    if (m) editor.setModel(m);
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => void save());
  };

  const missing = file.error instanceof ApiError && file.error.status === 404;
  const unsupported = file.data && (file.data.binary || file.data.tooLarge);

  return (
    <div className="flex h-full min-h-0 flex-col" data-testid="file-editor">
      <div className="flex h-8 shrink-0 items-center gap-2 border-b px-3 text-xs">
        <span className="truncate font-mono text-foreground" title={tab.path}>{tab.path}</span>
        {tab.dirty && <span className="text-sky-300">unsaved</span>}
        {status && (
          <span
            className={cn(
              "truncate",
              status.kind === "info" ? "text-muted-foreground" : status.kind === "error" ? "text-destructive" : "text-warning",
            )}
          >
            {status.text}
          </span>
        )}
        {status?.kind === "conflict" && (
          <>
            <Button size="sm" variant="outline" onClick={() => void save(true)}>Overwrite</Button>
            <Button size="sm" variant="ghost" onClick={() => void reload()}>Reload</Button>
          </>
        )}
        {status?.kind === "external" && (
          <Button size="sm" variant="ghost" onClick={() => void reload()}>Reload from disk</Button>
        )}
        <span className="ml-auto" />
        <Tip label={tab.diff ? "Back to the editor" : "Diff against HEAD"}>
          <Button size="sm" variant={tab.diff ? "secondary" : "ghost"} onClick={() => setDiff(pid, tab.key, !tab.diff)} data-testid="toggle-diff">
            <FileDiff /> Diff
          </Button>
        </Tip>
        <Tip label="Save (Ctrl+S)">
          <Button size="sm" variant="ghost" disabled={!tab.dirty || saving || missing} onClick={() => void save()} data-testid="save">
            {saving ? <Loader2 className="animate-spin" /> : <Save />} Save
          </Button>
        </Tip>
      </div>
      <div className="relative min-h-0 flex-1">
        {file.isLoading && <Centered><Loader2 className="size-4 animate-spin" /></Centered>}
        {file.error && !missing && <Centered className="text-destructive">{String(file.error)}</Centered>}
        {unsupported && <Centered>{file.data!.binary ? "Binary file: not shown." : "Too large to edit here (over 5 MB)."}</Centered>}
        {tab.diff && (file.data || missing) && !unsupported ? (
          head.isLoading ? (
            <Centered><Loader2 className="size-4 animate-spin" /></Centered>
          ) : head.data?.binary ? (
            <Centered>Binary at HEAD: no diff.</Centered>
          ) : (
            <DiffEditor
              original={head.data?.content ?? ""}
              modified={missing ? "" : (model()?.getValue() ?? "")}
              language={languageOf(tab.path)}
              theme="openorca"
              options={{ ...DIFF_OPTIONS, readOnly: true }}
            />
          )
        ) : (
          file.data && !unsupported && (
            <Editor
              path={(model(), modelUri(pid, tab).toString())}
              theme="openorca"
              language={languageOf(tab.path)}
              options={EDITOR_OPTIONS}
              keepCurrentModel
              onMount={onMount}
            />
          )
        )}
        {missing && !tab.diff && <Centered>The file isn't on disk (deleted?). Use Diff to see it at HEAD.</Centered>}
      </div>
    </div>
  );
}

function Centered({ children, className }: { children: React.ReactNode; className?: string }) {
  return <div className={cn("absolute inset-0 flex items-center justify-center p-4 text-center text-xs text-muted-foreground", className)}>{children}</div>;
}
