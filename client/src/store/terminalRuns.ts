import { create } from "zustand";

/**
 * Commands the UI asks a project's terminal to run (an engine's login), and how they ended.
 * The terminal types the command into its shell followed by a marker (an OSC 777 sequence
 * carrying the exit code) and reports the code here when the marker comes back.
 */
export interface TerminalRun {
  nonce: number;
  command: string;
  /** Exit code once the command finished; undefined while it runs. */
  code?: number;
}

interface TerminalRuns {
  runs: Record<string, TerminalRun>;
  request: (pid: string, command: string) => number;
  finish: (pid: string, nonce: number, code: number) => void;
  /** The terminal of this project has gone: a run in flight will never report back. */
  abandon: (pid: string) => void;
}

let nonce = 1;

export const useTerminalRuns = create<TerminalRuns>((set) => ({
  runs: {},
  request: (pid, command) => {
    const n = nonce++;
    set((s) => ({ runs: { ...s.runs, [pid]: { nonce: n, command } } }));
    return n;
  },
  finish: (pid, n, code) =>
    set((s) => (s.runs[pid]?.nonce === n ? { runs: { ...s.runs, [pid]: { ...s.runs[pid], code } } } : s)),
  abandon: (pid) =>
    set((s) => (s.runs[pid] && s.runs[pid].code === undefined ? { runs: { ...s.runs, [pid]: { ...s.runs[pid], code: -1 } } } : s)),
}));

/** The OSC number the end-of-command marker uses. */
export const RUN_MARKER_OSC = 777;

/** What the shell is sent: the command, then a printf of the marker with the exit code. */
export function markedCommand(command: string, n: number): string {
  return `${command}; printf '\\033]${RUN_MARKER_OSC};orca-run;${n};%d\\007' "$?"\r`;
}

/** The run number and exit code from a marker's payload ("orca-run;<n>;<code>"), or null. */
export function parseMarker(payload: string): { n: number; code: number } | null {
  const m = /^orca-run;(\d+);(-?\d+)$/.exec(payload);
  return m ? { n: Number(m[1]), code: Number(m[2]) } : null;
}
