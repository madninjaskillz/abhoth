import { useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { RotateCcw, SquareTerminal } from "lucide-react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { projectApi } from "@/lib/api";
import { useProject } from "@/store/session";

type State = "connecting" | "open" | "closed";

/**
 * A real shell on the server (ConPTY) in the project's folder, over a WebSocket.
 * Output arrives as binary frames; input and resizes go as small JSON text frames.
 */
export function TerminalPanel({ visible = true }: { visible?: boolean }) {
  const { id: pid } = useProject();
  const host = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const [state, setState] = useState<State>("connecting");
  const [generation, setGeneration] = useState(0);

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const term = new Terminal({
      fontFamily: '"JetBrains Mono", "Cascadia Code", Consolas, monospace',
      fontSize: 13,
      lineHeight: 1.15,
      cursorBlink: true,
      allowProposedApi: true,
      scrollback: 5000,
      theme: {
        background: "#0b0d10",
        foreground: "#c9d1d9",
        cursor: "#58a6ff",
        selectionBackground: "#264f78",
        black: "#484f58", red: "#ff7b72", green: "#3fb950", yellow: "#d29922", blue: "#58a6ff", magenta: "#bc8cff", cyan: "#39c5cf", white: "#b1bac4",
        brightBlack: "#6e7681", brightRed: "#ffa198", brightGreen: "#56d364", brightYellow: "#e3b341", brightBlue: "#79c0ff", brightMagenta: "#d2a8ff", brightCyan: "#56d4dd", brightWhite: "#f0f6fc",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      /* canvas renderer fallback */
    }
    try { fit.fit(); } catch { /* hidden */ }
    termRef.current = term;
    fitRef.current = fit;

    const ws = new WebSocket(projectApi(pid).ptyUrl(term.cols, term.rows));
    ws.binaryType = "arraybuffer";
    wsRef.current = ws;
    setState("connecting");
    ws.onopen = () => {
      setState("open");
      // The panel may have been laid out (or resized) while connecting: tell the PTY the real size.
      try { fit.fit(); } catch { /* hidden */ }
      ws.send(JSON.stringify({ t: "r", c: term.cols, r: term.rows }));
    };
    ws.onmessage = (e) => {
      if (e.data instanceof ArrayBuffer) term.write(new Uint8Array(e.data));
      else term.write(String(e.data));
    };
    ws.onclose = () => {
      setState("closed");
      term.write("\r\n\x1b[90m[terminal closed]\x1b[0m\r\n");
    };
    const input = term.onData((d) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ t: "i", d }));
    });
    const resize = term.onResize(({ cols, rows }) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ t: "r", c: cols, r: rows }));
    });

    const ro = new ResizeObserver(() => {
      if (el.offsetWidth > 0 && el.offsetHeight > 0) {
        try { fit.fit(); } catch { /* ignore */ }
      }
    });
    ro.observe(el);

    return () => {
      ro.disconnect();
      input.dispose();
      resize.dispose();
      ws.onclose = null;
      ws.close();
      term.dispose();
      termRef.current = null;
    };
  }, [generation, pid]);

  useEffect(() => {
    if (visible) {
      try { fitRef.current?.fit(); } catch { /* ignore */ }
      termRef.current?.focus();
    }
  }, [visible]);

  return (
    <div className="flex h-full min-h-0 flex-col bg-[#0b0d10]">
      <div className="flex h-8 shrink-0 items-center gap-2 border-b bg-panel px-3 text-xs">
        <SquareTerminal className="size-3.5 text-muted-foreground" />
        <span className="font-medium">Terminal</span>
        <span className={cn("size-1.5 rounded-full", state === "open" ? "bg-success" : state === "connecting" ? "bg-warning" : "bg-muted-foreground")} />
        <span className="ml-auto" />
        {state === "closed" && (
          <Button size="sm" variant="ghost" onClick={() => setGeneration((g) => g + 1)}>
            <RotateCcw /> New shell
          </Button>
        )}
      </div>
      <div ref={host} className="xterm-host min-h-0 flex-1" data-testid="terminal" />
    </div>
  );
}
