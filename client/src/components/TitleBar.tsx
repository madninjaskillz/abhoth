import type { ReactNode, MouseEvent as ReactMouseEvent } from "react";
import { MascotBadge } from "@/components/Mascot";
import { brandName } from "@/lib/brand";
import { useDesktopState, useTitleRowClaimed, windowCommands, type ResizeDir } from "@/lib/desktop";
import { isDragHit, makeDoubleClick, type HitNode } from "@/lib/titleRow";
import { cn } from "@/lib/utils";

/**
 * The page, and in the desktop app's frameless window (D143) a title bar for it. The app's own
 * header is the title row there (D144: tabs, status and buttons share it with the window
 * buttons); this separate bar is only for pages without that header (sign-in, setup, signed out).
 * The window keeps its DWM frame, so the left, right and bottom edges resize natively; the top
 * edge and corners resize from a thin strip in the row. The wrapper is always there so switching
 * the title bar doesn't remount the app.
 */
export function WindowFrame({ children }: { children: ReactNode }) {
  const state = useDesktopState();
  const claimed = useTitleRowClaimed();
  const bar = !!state?.frameless && !claimed;
  return (
    <div className="flex h-full flex-col">
      {bar && <TitleBar maximized={state.maximized} focused={state.focused} />}
      <div className="relative min-h-0 flex-1">{children}</div>
    </div>
  );
}

function TitleBar({ maximized, focused }: { maximized: boolean; focused: boolean }) {
  return (
    <div
      className={cn("relative flex h-8 shrink-0 select-none items-stretch border-b bg-panel text-xs", !focused && "text-muted-foreground")}
      onMouseDown={onTitleRowPress}
      data-testid="title-bar"
      data-drag-region
      data-maximized={maximized}
    >
      <div className="flex min-w-0 flex-1 items-center gap-2 pl-2.5" data-testid="title-bar-drag">
        <MascotBadge className={cn("size-[18px]", !focused && "opacity-70")} />
        <span className={cn("truncate font-semibold", focused && "text-primary")}>{brandName()}</span>
      </div>
      <WindowControls maximized={maximized} />
      {!maximized && <ResizeStrip />}
    </div>
  );
}

const secondPress = makeDoubleClick();

/** The elements from `target` up to `row`, for the drag hit test; null when `target` is outside the row (a portal). */
function hitChain(target: EventTarget | null, row: Element): HitNode[] | null {
  const chain: HitNode[] = [];
  for (let el = target instanceof Element ? target : null; el; el = el.parentElement) {
    chain.push({ tag: el.tagName, role: el.getAttribute("role"), noDrag: el.hasAttribute("data-no-drag"), dragRegion: el.hasAttribute("data-drag-region") });
    if (el === row) return chain;
  }
  return null;
}

/**
 * A press on a title row: on its empty space (a `data-drag-region` with nothing clickable under
 * the pointer) it moves the window, and a second press maximises or restores. Anything else
 * (tabs, buttons, menus, dialogs in portals) is left alone.
 */
export function onTitleRowPress(e: ReactMouseEvent) {
  if (e.button !== 0) return;
  const chain = hitChain(e.target, e.currentTarget);
  if (!chain || !isDragHit(chain)) return;
  e.preventDefault();
  if (secondPress(e.screenX, e.screenY, e.timeStamp)) windowCommands.toggleMaximize();
  else windowCommands.drag();
}

/** Minimise, maximise/restore and close (to the tray, as the OS button). Above dialogs' overlays, as an OS title bar is. */
export function WindowControls({ maximized }: { maximized: boolean }) {
  return (
    <div className="pointer-events-auto relative z-[60] flex shrink-0 items-stretch self-stretch" data-window-chrome data-no-drag data-testid="window-controls">
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
    </div>
  );
}

function CaptionButton({ label, onClick, children, close, testId }: { label: string; onClick: () => void; children: ReactNode; close?: boolean; testId: string }) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
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
export function ResizeStrip() {
  const start = (dir: ResizeDir) => (e: ReactMouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    windowCommands.resize(dir);
  };
  return (
    <>
      <div className="absolute inset-x-2 top-0 z-[61] h-[4px] cursor-ns-resize" onMouseDown={start("n")} data-no-drag aria-hidden data-testid="resize-top" />
      <div className="absolute left-0 top-0 z-[61] size-2 cursor-nwse-resize" onMouseDown={start("nw")} data-no-drag aria-hidden />
      <div className="absolute right-0 top-0 z-[61] size-2 cursor-nesw-resize" onMouseDown={start("ne")} data-no-drag aria-hidden />
    </>
  );
}
