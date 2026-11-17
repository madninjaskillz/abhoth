import { loader } from "@monaco-editor/react";
import * as monaco from "monaco-editor/esm/vs/editor/editor.api";
import "monaco-editor/esm/vs/basic-languages/monaco.contribution";
import EditorWorker from "monaco-editor/esm/vs/editor/editor.worker?worker";

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

export { monaco };

const FONT = '"JetBrains Mono", "Cascadia Code", Consolas, monospace';

export const EDITOR_OPTIONS: monaco.editor.IStandaloneEditorConstructionOptions = {
  minimap: { enabled: false },
  fontSize: 13,
  fontFamily: FONT,
  scrollBeyondLastLine: false,
  automaticLayout: true,
  renderWhitespace: "selection",
  tabSize: 4,
  detectIndentation: true,
};

export const DIFF_OPTIONS: monaco.editor.IDiffEditorConstructionOptions = {
  renderSideBySide: true,
  useInlineViewWhenSpaceIsLimited: true,
  minimap: { enabled: false },
  fontSize: 12,
  fontFamily: FONT,
  scrollBeyondLastLine: false,
  automaticLayout: true,
  renderOverviewRuler: false,
  hideUnchangedRegions: { enabled: true },
};
