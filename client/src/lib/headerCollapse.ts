// When the app bar's buttons fold into the menu (☰), and how the arrow keys move in that menu.
// Pure (no React, no DOM), so plain node can test it (test/headerCollapse.test.ts).

/** Viewports this narrow (phones) always use the menu: the bar's icons are too small to tap. */
export const PHONE_WIDTH = 480;
/** Extra room needed before a folded bar unfolds, so it doesn't flicker at the edge. */
export const UNFOLD_SLACK = 24;

export interface BarMeasure {
  /** The window's width. */
  viewport: number;
  /** The width the tabs and the buttons can share (the bar minus the logo, the status dot and gaps). */
  room: number;
  /** The tabs' natural width (all of them, plus the add button). */
  tabs: number;
  /** The buttons' width when laid out in the bar; 0 when not measured yet. */
  actions: number;
  /** Whether the buttons are folded now. */
  collapsed: boolean;
}

/** Whether the bar's buttons fold into the menu: on a phone, or when the tabs and buttons don't both fit. */
export function shouldCollapse(m: BarMeasure): boolean {
  if (m.viewport <= PHONE_WIDTH) return true;
  // Folded without knowing the buttons' width: unfold once to measure it.
  if (m.actions <= 0) return false;
  const need = m.tabs + m.actions;
  return m.collapsed ? need + UNFOLD_SLACK > m.room : need > m.room;
}

/** The menu item an arrow key moves to (wrapping; Home and End jump); the same one for any other key. */
export function nextMenuIndex(current: number, count: number, key: string): number {
  if (count <= 0) return -1;
  switch (key) {
    case "ArrowDown":
      return current < 0 ? 0 : (current + 1) % count;
    case "ArrowUp":
      return current < 0 ? count - 1 : (current - 1 + count) % count;
    case "Home":
      return 0;
    case "End":
      return count - 1;
    default:
      return current;
  }
}
