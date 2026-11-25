// The hub connection the app uses, over either transport:
//  * SignalR at /hub/session (the C# host);
//  * one plain websocket at /hub/ws (the Rust host, which says `hub: "ws"` in /api/info).
// Both offer the small part of SignalR's HubConnection the app needs: invoke, on/off, start/stop
// and the reconnect events. The websocket protocol (JSON, numbered server messages):
//   client: {type:"invoke",id,method,args} · {type:"ack",n} · {type:"close"}
//   server: {type:"welcome",connectionId,resumed} · {type:"ping"} ·
//           {type:"result",id,n,result|error} · {type:"event",n,method,args}
// After a drop it reconnects with ?resume=<connectionId>&last=<n>: within the server's grace
// period it gets every message it missed (answers to calls in flight included) and nothing has
// to be reloaded; otherwise it starts fresh and `onreconnected` fires as with SignalR.
// (No imports: node's test runner loads this file as it is.)

export type HubState = "Disconnected" | "Connecting" | "Connected" | "Reconnecting";

export interface HubLike {
  readonly state: HubState;
  invoke<T = unknown>(method: string, ...args: unknown[]): Promise<T>;
  on(method: string, handler: (...args: any[]) => void): void;
  off(method: string): void;
  start(): Promise<void>;
  stop(): Promise<void>;
  onreconnecting(cb: (error?: Error) => void): void;
  onreconnected(cb: (connectionId?: string) => void): void;
  onclose(cb: (error?: Error) => void): void;
  /** The websocket transport only: a reconnect that resumed (nothing was missed). */
  onresumed?(cb: () => void): void;
}

/** What the adapter needs of a WebSocket (the browser's, or a fake in tests). */
export interface SocketLike {
  readyState: number;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  send(data: string): void;
  close(): void;
}

export interface WsHubOptions {
  /** ws:// or wss:// URL of /hub/ws. */
  url: string;
  createSocket?: (url: string) => SocketLike;
  /** Delay before reconnect attempt `n` (0-based); null gives up. */
  retryDelay?: (attempt: number) => number | null;
  /** Reconnect when nothing (not even a ping) arrives for this long. */
  serverTimeoutMs?: number;
  /** Acknowledge received messages after this many (the server then forgets them). */
  ackEvery?: number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

interface Pending {
  resolve: (v: any) => void;
  reject: (e: Error) => void;
}

const OPEN = 1;

export class WsHubConnection implements HubLike {
  state: HubState = "Disconnected";
  connectionId: string | null = null;
  private socket: SocketLike | null = null;
  private last = 0;
  private acked = 0;
  private nextId = 0;
  private readonly pending = new Map<string, Pending>();
  private readonly handlers = new Map<string, (...args: any[]) => void>();
  private readonly reconnecting: ((e?: Error) => void)[] = [];
  private readonly reconnected: ((id?: string) => void)[] = [];
  private readonly resumedCbs: (() => void)[] = [];
  private readonly closed: ((e?: Error) => void)[] = [];
  private stopped = false;
  private attempt = 0;
  private watchdog: unknown = null;
  private retryTimer: unknown = null;
  private readonly o: Required<WsHubOptions>;

  constructor(options: WsHubOptions) {
    this.o = {
      createSocket: (url) => new WebSocket(url) as unknown as SocketLike,
      retryDelay: (n) => Math.min(10000, 500 * 2 ** n),
      serverTimeoutMs: 60000,
      ackEvery: 50,
      setTimer: (fn, ms) => setTimeout(fn, ms),
      clearTimer: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
      ...options,
    };
  }

  on(method: string, handler: (...args: any[]) => void) {
    this.handlers.set(method, handler);
  }
  off(method: string) {
    this.handlers.delete(method);
  }
  onreconnecting(cb: (e?: Error) => void) {
    this.reconnecting.push(cb);
  }
  onreconnected(cb: (id?: string) => void) {
    this.reconnected.push(cb);
  }
  onresumed(cb: () => void) {
    this.resumedCbs.push(cb);
  }
  onclose(cb: (e?: Error) => void) {
    this.closed.push(cb);
  }

  start(): Promise<void> {
    this.stopped = false;
    this.state = "Connecting";
    return new Promise((resolve, reject) => this.open(false, resolve, reject));
  }

  async stop(): Promise<void> {
    this.stopped = true;
    if (this.retryTimer !== null) this.o.clearTimer(this.retryTimer);
    this.retryTimer = null;
    const s = this.socket;
    this.socket = null;
    if (s) {
      try {
        if (s.readyState === OPEN) s.send(JSON.stringify({ type: "close" }));
        s.close();
      } catch {
        /* already gone */
      }
    }
    this.finish(new Error("Connection stopped."));
  }

  invoke<T = unknown>(method: string, ...args: unknown[]): Promise<T> {
    if (this.state !== "Connected" || !this.socket) return Promise.reject(new Error("Not connected."));
    const id = String(++this.nextId);
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      try {
        this.socket!.send(JSON.stringify({ type: "invoke", id, method, args }));
      } catch (e) {
        this.pending.delete(id);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  private open(resume: boolean, resolve?: () => void, reject?: (e: Error) => void) {
    let url = this.o.url;
    if (resume && this.connectionId) {
      url += (url.includes("?") ? "&" : "?") + `resume=${encodeURIComponent(this.connectionId)}&last=${this.last}`;
    }
    let welcomed = false;
    let socket: SocketLike;
    try {
      socket = this.o.createSocket(url);
    } catch (e) {
      const err = e instanceof Error ? e : new Error(String(e));
      if (reject) reject(err);
      else this.retry(err);
      return;
    }
    this.socket = socket;
    socket.onmessage = (ev) => {
      if (this.socket !== socket) return;
      this.kick();
      let msg: any;
      try {
        msg = JSON.parse(String(ev.data));
      } catch {
        return;
      }
      if (msg.type === "welcome") {
        welcomed = true;
        const resumed = !!msg.resumed && resume;
        if (!resumed) {
          // A fresh connection: numbering starts over, calls in flight were lost.
          this.last = 0;
          this.acked = 0;
          this.failPending(new Error("The connection was lost; try again."));
        }
        this.connectionId = msg.connectionId;
        this.state = "Connected";
        this.attempt = 0;
        if (resolve) resolve();
        else if (resumed) this.resumedCbs.forEach((cb) => cb());
        else this.reconnected.forEach((cb) => cb(this.connectionId ?? undefined));
        return;
      }
      if (typeof msg.n === "number") {
        if (msg.n <= this.last) return; // a replay of something already handled
        this.last = msg.n;
        if (this.last - this.acked >= this.o.ackEvery) {
          this.acked = this.last;
          try {
            socket.send(JSON.stringify({ type: "ack", n: this.last }));
          } catch {
            /* the close handler reconnects */
          }
        }
      }
      if (msg.type === "result") {
        const p = this.pending.get(msg.id);
        if (!p) return;
        this.pending.delete(msg.id);
        if (typeof msg.error === "string") p.reject(new Error(msg.error));
        else p.resolve(msg.result ?? null);
      } else if (msg.type === "event") {
        const h = this.handlers.get(msg.method);
        if (h) {
          try {
            h(...(msg.args ?? []));
          } catch (e) {
            console.error(e);
          }
        }
      }
    };
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.clearWatchdog();
      if (this.stopped) return;
      if (!welcomed && reject) {
        this.state = "Disconnected";
        reject(new Error("Could not connect to the hub."));
        return;
      }
      this.retry(new Error("The connection was lost."));
    };
    socket.onerror = () => {
      /* onclose follows */
    };
    socket.onopen = () => this.kick();
  }

  private retry(error: Error) {
    if (this.stopped) return;
    if (this.state !== "Reconnecting") {
      this.state = "Reconnecting";
      this.reconnecting.forEach((cb) => cb(error));
    }
    const delay = this.o.retryDelay(this.attempt++);
    if (delay === null) {
      this.finish(error);
      return;
    }
    this.retryTimer = this.o.setTimer(() => {
      this.retryTimer = null;
      if (!this.stopped) this.open(true);
    }, delay);
  }

  /** Nothing heard for serverTimeoutMs: drop the socket (the close handler reconnects). */
  private kick() {
    this.clearWatchdog();
    const socket = this.socket;
    this.watchdog = this.o.setTimer(() => {
      if (this.socket !== socket || !socket) return;
      this.socket = null;
      try {
        socket.close();
      } catch {
        /* gone */
      }
      this.retry(new Error("Server timeout."));
    }, this.o.serverTimeoutMs);
  }

  private clearWatchdog() {
    if (this.watchdog !== null) this.o.clearTimer(this.watchdog);
    this.watchdog = null;
  }

  private failPending(error: Error) {
    for (const p of this.pending.values()) p.reject(error);
    this.pending.clear();
  }

  private finish(error: Error) {
    this.clearWatchdog();
    this.failPending(error);
    const was = this.state;
    this.state = "Disconnected";
    if (was !== "Disconnected") this.closed.forEach((cb) => cb(error));
  }
}

/** The websocket URL of /hub/ws on the page's own host. */
export function hubSocketUrl(loc: { protocol: string; host: string }): string {
  return `${loc.protocol === "https:" ? "wss" : "ws"}://${loc.host}/hub/ws`;
}

/** Which hub the server offers: the Rust host says `hub: "ws"` in /api/info; C# says nothing. */
export function transportOf(info: unknown): "ws" | "signalr" {
  return info && typeof info === "object" && (info as { hub?: unknown }).hub === "ws" ? "ws" : "signalr";
}
