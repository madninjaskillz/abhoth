import { useEffect, useSyncExternalStore } from "react";

/**
 * The desktop app's window bridge (D143): `window.abhothDesktop`, injected by abhoth-desktop
 * into the app's own top frame only (not in a browser, not in frames). Commands go to the
 * window over wry IPC with a per-launch nonce the bridge keeps to itself.
 */
export interface DesktopState {
  /** The window has no OS frame: the page draws the title bar. */
  frameless: boolean;
  maximized: boolean;
  focused: boolean;
}

export type ResizeDir = "n" | "ne" | "nw" | "s" | "se" | "sw" | "e" | "w";

interface Bridge {
  platform: "windows" | "macos" | "linux";
  state(): DesktopState;
  subscribe(fn: (s: DesktopState) => void): () => void;
  send(cmd: string, extra?: Record<string, unknown>): void;
}

export function desktopBridge(): Bridge | null {
  const b = (window as unknown as { abhothDesktop?: Bridge }).abhothDesktop;
  return b && typeof b.send === "function" ? b : null;
}

/** Running inside the desktop app. */
export function inDesktop(): boolean {
  return desktopBridge() !== null;
}

export const windowCommands = {
  minimize: () => desktopBridge()?.send("minimize"),
  toggleMaximize: () => desktopBridge()?.send("maximize"),
  /** As the OS close button: to the tray (or quit with desktop.closeToTray off). */
  close: () => desktopBridge()?.send("close"),
  drag: () => desktopBridge()?.send("drag"),
  resize: (dir: ResizeDir) => desktopBridge()?.send("resize", { dir }),
  /** The OS title bar (true) or the app's own (false), at once. */
  systemTitleBar: (on: boolean) => desktopBridge()?.send("systemTitleBar", { value: on }),
};

const none = () => () => {};
let asked = false;

/** The window's state (null outside the desktop app); asks the window for it once per page. */
export function useDesktopState(): DesktopState | null {
  const bridge = desktopBridge();
  const state = useSyncExternalStore(bridge ? (fn) => bridge.subscribe(fn) : none, () => bridge?.state() ?? null);
  useEffect(() => {
    if (bridge && !asked) {
      asked = true;
      bridge.send("state");
    }
  }, [bridge]);
  return state;
}

// The second part of the title bar's text (the open project), set by the app.
let subtitle = "";
const subtitleSubs = new Set<() => void>();

export function setWindowSubtitle(text: string) {
  if (text === subtitle) return;
  subtitle = text;
  subtitleSubs.forEach((f) => f());
}

export function useWindowSubtitle(): string {
  return useSyncExternalStore(
    (fn) => {
      subtitleSubs.add(fn);
      return () => subtitleSubs.delete(fn);
    },
    () => subtitle,
  );
}
