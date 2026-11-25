// The websocket hub adapter (the Rust host's /hub/ws): invoke/results, events, acks, reconnect
// with resume, a fresh reconnect, stop, and the transport pick from /api/info.
import { test } from "node:test";
import assert from "node:assert/strict";
import { WsHubConnection, hubSocketUrl, transportOf, type SocketLike } from "../src/lib/hubConnection.ts";

class FakeSocket implements SocketLike {
  readyState = 0;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  sent: any[] = [];
  closed = false;
  url: string;
  constructor(url: string) {
    this.url = url;
  }
  send(data: string) {
    this.sent.push(JSON.parse(data));
  }
  close() {
    this.closed = true;
  }
  open() {
    this.readyState = 1;
    this.onopen?.({});
  }
  receive(msg: unknown) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
  drop() {
    this.readyState = 3;
    this.onclose?.({});
  }
}

function harness(opts: { ackEvery?: number } = {}) {
  const sockets: FakeSocket[] = [];
  const timers: { fn: () => void; ms: number; live: boolean }[] = [];
  const hub = new WsHubConnection({
    url: "ws://h/hub/ws",
    createSocket: (url) => {
      const s = new FakeSocket(url);
      sockets.push(s);
      return s;
    },
    retryDelay: (n) => (n < 3 ? 10 : null),
    ackEvery: opts.ackEvery ?? 50,
    setTimer: (fn, ms) => {
      const t = { fn, ms, live: true };
      timers.push(t);
      return t;
    },
    clearTimer: (h) => {
      (h as { live: boolean }).live = false;
    },
  });
  /** Run the pending timers of this length (retry delays are 10 ms, the watchdog 60 s). */
  const fire = (ms: number) => {
    for (const t of timers.filter((t) => t.live && t.ms === ms)) {
      t.live = false;
      t.fn();
    }
  };
  return { hub, sockets, fire, last: () => sockets[sockets.length - 1] };
}

const tick = () => new Promise((r) => setImmediate(r));

test("invoke resolves with the result, rejects with the hub's error", async () => {
  const { hub, last } = harness();
  const started = hub.start();
  last().open();
  last().receive({ type: "welcome", connectionId: "c1", resumed: false });
  await started;
  assert.equal(hub.state, "Connected");

  const a = hub.invoke<string>("SendPrompt", "p1", "hi");
  const b = hub.invoke("SendPrompt", "nope", "x");
  const [ia, ib] = last().sent.map((m) => m.id);
  assert.deepEqual(last().sent[0], { type: "invoke", id: ia, method: "SendPrompt", args: ["p1", "hi"] });
  last().receive({ type: "result", id: ia, n: 1, result: null });
  last().receive({ type: "result", id: ib, n: 2, error: "No such project." });
  assert.equal(await a, null);
  await assert.rejects(b, /No such project\./);
});

test("events reach their handler; replays of seen messages are dropped; acks go out", async () => {
  const { hub, last } = harness({ ackEvery: 2 });
  const got: unknown[][] = [];
  hub.on("events", (...args) => got.push(args));
  const started = hub.start();
  last().receive({ type: "welcome", connectionId: "c1", resumed: false });
  await started;
  last().receive({ type: "event", n: 1, method: "events", args: ["p1", [{ seq: 1 }]] });
  last().receive({ type: "event", n: 1, method: "events", args: ["p1", [{ seq: 1 }]] });
  last().receive({ type: "ping" });
  last().receive({ type: "event", n: 2, method: "events", args: ["p1", [{ seq: 2 }]] });
  assert.deepEqual(got, [["p1", [{ seq: 1 }]], ["p1", [{ seq: 2 }]]]);
  assert.deepEqual(last().sent, [{ type: "ack", n: 2 }]);
});

test("a drop reconnects with resume; calls in flight get their answers from the replay", async () => {
  const { hub, sockets, fire, last } = harness();
  const states: string[] = [];
  hub.onreconnecting(() => states.push("reconnecting"));
  hub.onreconnected(() => states.push("reconnected"));
  hub.onresumed(() => states.push("resumed"));
  const started = hub.start();
  last().receive({ type: "welcome", connectionId: "c1", resumed: false });
  await started;
  last().receive({ type: "event", n: 1, method: "projects", args: [] });
  const call = hub.invoke("GetSnapshot", "p1");
  const id = last().sent[0].id;
  last().drop();
  assert.equal(hub.state, "Reconnecting");
  await assert.rejects(hub.invoke("Cancel", "p1"), /Not connected/);
  fire(10);
  assert.equal(sockets.length, 2);
  assert.equal(last().url, "ws://h/hub/ws?resume=c1&last=1");
  last().receive({ type: "welcome", connectionId: "c1", resumed: true });
  last().receive({ type: "result", id, n: 2, result: { busy: false } });
  assert.deepEqual(await call, { busy: false });
  assert.deepEqual(states, ["reconnecting", "resumed"]);
  assert.equal(hub.state, "Connected");
});

test("a reconnect the server can't resume starts over: lost calls fail, onreconnected fires", async () => {
  const { hub, fire, last } = harness();
  let reconnected = 0;
  hub.onreconnected(() => reconnected++);
  const started = hub.start();
  last().receive({ type: "welcome", connectionId: "c1", resumed: false });
  await started;
  last().receive({ type: "event", n: 5, method: "projects", args: [] });
  const call = hub.invoke("GetSnapshot", "p1");
  last().drop();
  fire(10);
  last().receive({ type: "welcome", connectionId: "c2", resumed: false });
  await assert.rejects(call, /connection was lost/);
  assert.equal(reconnected, 1);
  assert.equal(hub.connectionId, "c2");
  // Numbering restarted: message 1 of the new connection is not a replay.
  let seen = 0;
  hub.on("projects", () => seen++);
  last().receive({ type: "event", n: 1, method: "projects", args: [] });
  assert.equal(seen, 1);
});

test("silence past the server timeout reconnects; giving up closes", async () => {
  const { hub, fire, sockets, last } = harness();
  let closed = 0;
  hub.onclose(() => closed++);
  const started = hub.start();
  last().receive({ type: "welcome", connectionId: "c1", resumed: false });
  await started;
  fire(60000);
  assert.equal(sockets[0].closed, true);
  assert.equal(hub.state, "Reconnecting");
  for (let i = 0; i < 3; i++) {
    fire(10);
    last().drop();
  }
  await tick();
  assert.equal(hub.state, "Disconnected");
  assert.equal(closed, 1);
});

test("stop says close and rejects what is pending", async () => {
  const { hub, last } = harness();
  const started = hub.start();
  last().open();
  last().receive({ type: "welcome", connectionId: "c1", resumed: false });
  await started;
  const call = hub.invoke("GetSnapshot", "p1");
  await hub.stop();
  assert.deepEqual(last().sent.at(-1), { type: "close" });
  await assert.rejects(call);
  assert.equal(hub.state, "Disconnected");
});

test("a first connect that fails rejects start", async () => {
  const { hub, last } = harness();
  const started = hub.start();
  last().drop();
  await assert.rejects(started, /Could not connect/);
});

test("transport and URL", () => {
  assert.equal(transportOf({ hub: "ws" }), "ws");
  assert.equal(transportOf({ version: "1" }), "signalr");
  assert.equal(transportOf(null), "signalr");
  assert.equal(hubSocketUrl({ protocol: "https:", host: "x:1" }), "wss://x:1/hub/ws");
  assert.equal(hubSocketUrl({ protocol: "http:", host: "x" }), "ws://x/hub/ws");
});
