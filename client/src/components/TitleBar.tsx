import type { ReactNode, MouseEvent as ReactMouseEvent } from "react";
import { MascotBadge } from "@/components/Mascot";
import { brandName } from "@/lib/brand";
import { useDesktopState, useWindowSubtitle, windowCommands, type ResizeDir } from "@/lib/desktop";
import { cn } from "@/lib/utils";

/**
 * The page and, in the desktop app's frameless window (D143), its title bar above it: the mascot,
 * the app and project name, a drag area (double-click maximises), and minimise, maximise/restore
 * and close (to the tray, as the OS button). The window keeps its DWM frame, so the left, right
 * and bottom edges resize natively; the top edge and corners resize from a thin strip here.
 * The frame wrapper is always there so switching the title bar doesn't remount the app.
 */
export function WindowFrame({ children }: { children: ReactNode }) {
  const state = useDesktopState();
  const bar = !!state?.frameless;
  return (
    <div className="flex h-full flex-col">
      {bar && <TitleBar maximized={state.maximized} focused={state.focused} />}
      <div className="relative min-h-0 flex-1">{children}</div>
    </div>
  );
}

function TitleBar({ maximized, focused }: { maximized: boolean; focused: boolean }) {
  const subtitle = useWindowSubtitle();
  const onDragArea = (e: ReactMouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    // The window's move loop takes the first press's release, so the browser's click count
    // (dblclick, e.detail) isn't reliable here: count the second press ourselves.
    if (isSecondPress(e.screenX, e.screenY, e.timeStamp)) windowCommands.toggleMaximize();
    else windowCommands.drag();
  };
  return (
    <div
      // Above dialogs' overlays (z-50) and clickable while one is open, as an OS title bar is.
      className={cn("pointer-events-auto relative z-[60] flex h-8 shrink-0 select-none items-stretch border-b bg-panel text-xs", !focused && "text-muted-foreground")}
      data-testid="title-bar"
      data-window-chrome
      data-maximized={maximized}
    >
      <div className="flex min-w-0 flex-1 items-center gap-2 pl-2.5" onMouseDown={onDragArea} data-testid="title-bar-drag">
        <MascotBadge className={cn("size-[18px]", !focused && "opacity-70")} />
        <span className="truncate">
          <span className={cn("font-semibold", focused && "text-primary")}>{brandName()}</span>
          {subtitle && <span className="text-muted-foreground"> · {subtitle}</span>}
        </span>
      </div>
      <CaptionButton label="Minimize" onClick={windowCommands.minimize} testId="title-bar-minimize">
        <path d="M0 5.5h10" />
      </CaptionButton>
      <CaptionButton label={maximized ? "Restore Down" : "Maximize"} onClick={windowCommands.toggleMaximize} testId="title-bar-maximize">
        {maximized ? (
          <>
            <rect x="0.5" y="2.5" width="7" height="7" rx="1" />
            <path d="M2.5 2.5V1.5a1 1 0 0 1 1-1h5a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-1" />
          </>
        ) : (
          <rect x="0.5" y="0.5" width="9" height="9" rx="1" />
        )}
      </CaptionButton>
      <CaptionButton label="Close" onClick={windowCommands.close} testId="title-bar-close" close>
        <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" />
      </CaptionButton>
      {!maximized && <ResizeStrip />}
    </div>
  );
}

let lastPress: { x: number; y: number; t: number } | null = null;

/** A press within the double-click time (Windows' default, 500 ms) and a few pixels of the last one. */
function isSecondPress(x: number, y: number, t: number): boolean {
  const second = !!lastPress && t - lastPress.t < 500 && Math.abs(x - lastPress.x) <= 4 && Math.abs(y - lastPress.y) <= 4;
  lastPress = second ? null : { x, y, t };
  return second;
}

function CaptionButton({ label, onClick, children, close, testId }: { label: string; onClick: () => void; children: ReactNode; close?: boolean; testId: string }) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
      // Not a drag: a press on a button is a press.
      onMouseDown={(e) => e.stopPropagation()}
      className={cn(
        "flex w-[46px] shrink-0 items-center justify-center text-foreground/80 outline-none transition-colors focus-visible:bg-accent",
        close ? "hover:bg-[#c42b1c] hover:text-white active:bg-[#c42b1c]/80" : "hover:bg-accent active:bg-accent/70",
      )}
      data-testid={testId}
    >
      <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1" aria-hidden>
        {children}
      </svg>
    </button>
  );
}

/** The top edge and the top corners: the webview covers the frame there, so the page starts the resize. */
function ResizeStrip() {
  const start = (dir: ResizeDir) => (e: ReactMouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    windowCommands.resize(dir);
  };
  return (
    <>
      <div className="absolute inset-x-2 top-0 h-[4px] cursor-ns-resize" onMouseDown={start("n")} aria-hidden />
      <div className="absolute left-0 top-0 size-2 cursor-nwse-resize" onMouseDown={start("nw")} aria-hidden />
      <div className="absolute right-0 top-0 size-2 cursor-nesw-resize" onMouseDown={start("ne")} aria-hidden />
    </>
  );
}
