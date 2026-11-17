import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** Agent letters in palette order (as the TUI: Core/Orchestration/AgentPalette.cs). */
const LETTERS = "ABCDEFGHIJKMNOPQRSTUVWXYZ";
const COLORS = ["#3b8eea", "#3fb950", "#e3b341", "#a371f7", "#f0883e", "#f85149", "#b07a4a", "#d0d7de"];

/** The bar colour of an agent letter ("L" is the Lead, grey). */
export function agentColor(letter: string): string {
  if (!letter || letter === "L") return "#8b949e";
  const i = LETTERS.indexOf(letter.toUpperCase());
  return i < 0 ? "#8b949e" : COLORS[i % COLORS.length];
}

export function formatMs(ms?: number | null): string {
  if (ms == null) return "";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)} s`;
  return `${Math.floor(s / 60)}m ${Math.round(s % 60).toString().padStart(2, "0")}s`;
}

/** Monaco language id from a file name. */
export function languageOf(path: string): string {
  const lower = path.toLowerCase();
  if (lower.endsWith("dockerfile")) return "dockerfile";
  const ext = lower.split(".").pop() ?? "";
  const map: Record<string, string> = {
    ts: "typescript", tsx: "typescript", js: "javascript", jsx: "javascript", mjs: "javascript", cjs: "javascript",
    cs: "csharp", csx: "csharp", json: "json", md: "markdown", css: "css", scss: "scss", less: "less",
    html: "html", htm: "html", xml: "xml", csproj: "xml", props: "xml", targets: "xml",
    py: "python", rs: "rust", go: "go", java: "java", kt: "kotlin", rb: "ruby", php: "php",
    sh: "shell", bash: "shell", ps1: "powershell", psm1: "powershell", yml: "yaml", yaml: "yaml",
    sql: "sql", c: "c", h: "c", cpp: "cpp", hpp: "cpp", cc: "cpp", swift: "swift", lua: "lua", toml: "ini", ini: "ini",
  };
  return map[ext] ?? "plaintext";
}
