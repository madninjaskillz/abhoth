// The desktop app's title row (D144): which presses on it move the window, and the double-click
// that maximises. Pure (no React, no DOM types), so plain node can test it (test/titleRow.test.ts).

/** The drag space the row always keeps free of tabs and buttons (CSS px). */
export const DRAG_MIN = 48;

/** One element on the way from the pressed element up to the row (the row last). */
export interface HitNode {
  tag: string;
  role?: string | null;
  /** `data-no-drag`: never a drag, whatever it is. */
  noDrag?: boolean;
  /** `data-drag-region`: empty space here moves the window. */
  dragRegion?: boolean;
}

const INTERACTIVE_TAGS = new Set(["button", "a", "input", "select", "textarea", "label", "summary", "option"]);
const INTERACTIVE_ROLES = new Set(["tab", "button", "link", "menuitem", "menuitemcheckbox", "menu", "switch", "checkbox", "option", "combobox", "dialog", "textbox"]);

/**
 * Whether a press moves the window, like CSS `app-region: drag`: on empty space of a drag region,
 * never on anything clickable (or inside it). `chain` runs from the pressed element up to the
 * row; an element outside the row (a dialog in a portal) is not in it, so `inRow` is false.
 */
export function isDragHit(chain: HitNode[], inRow = true): boolean {
  if (!inRow) return false;
  for (const n of chain) {
    if (n.noDrag) return false;
    if (INTERACTIVE_TAGS.has(n.tag.toLowerCase())) return false;
    if (n.role && INTERACTIVE_ROLES.has(n.role)) return false;
    if (n.dragRegion) return true;
  }
  return false;
}

/** Windows' default double-click time and slop. */
export const DOUBLE_CLICK_MS = 500;
export const DOUBLE_CLICK_SLOP = 4;

/**
 * Counts double-clicks itself: the window's move loop takes the first press's release, so the
 * browser's dblclick (and `e.detail`) can't be trusted on a drag area.
 */
export function makeDoubleClick() {
  let last: { x: number; y: number; t: number } | null = null;
  return (x: number, y: number, t: number): boolean => {
    const second = !!last && t - last.t < DOUBLE_CLICK_MS && Math.abs(x - last.x) <= DOUBLE_CLICK_SLOP && Math.abs(y - last.y) <= DOUBLE_CLICK_SLOP;
    last = second ? null : { x, y, t };
    return second;
  };
}

/** The window title for the taskbar and Alt+Tab: the open project, then the product. */
export function windowTitle(product: string, project: string): string {
  const p = project.trim();
  return p ? `${p} · ${product}` : product;
}
