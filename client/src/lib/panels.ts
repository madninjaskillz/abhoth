import { useEffect, useRef, useState } from "react";

/**
 * Whether one of a project's side panels ("files", "git") is folded to its rail, remembered per
 * project in localStorage (openorca.<panel>Collapsed.<pid>).
 */
export function usePanelCollapsed(pid: string, panel: "files" | "git", byDefault: boolean): [boolean, (v: boolean) => void] {
  const key = `openorca.${panel}Collapsed.${pid}`;
  const [collapsed, setState] = useState(() => {
    try {
      const v = localStorage.getItem(key);
      return v === null ? byDefault : v === "1";
    } catch {
      return byDefault;
    }
  });
  const set = (v: boolean) => {
    setState(v);
    try {
      localStorage.setItem(key, v ? "1" : "0");
    } catch {
      /* private window */
    }
  };
  return [collapsed, set];
}

/**
 * Ctrl+B folds the files panel, Ctrl+Shift+G the git panel, while <paramref name="active"/>. Not
 * inside a terminal (Ctrl+B is the shell's), and nothing else on the page uses them.
 */
export function usePanelKeys(active: boolean, toggleFiles: () => void, toggleGit: () => void) {
  const handlers = useRef({ toggleFiles, toggleGit });
  handlers.current = { toggleFiles, toggleGit };
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || e.altKey) return;
      if (e.target instanceof Element && e.target.closest(".xterm")) return;
      const k = e.key.toLowerCase();
      if (k === "b" && !e.shiftKey) {
        e.preventDefault();
        handlers.current.toggleFiles();
      } else if (k === "g" && e.shiftKey) {
        e.preventDefault();
        handlers.current.toggleGit();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [active]);
}
