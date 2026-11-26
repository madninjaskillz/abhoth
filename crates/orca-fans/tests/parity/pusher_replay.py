#!/usr/bin/env python3
"""Replay a recorded nvidia-smi trace through the REAL push-gpu-fans.py, in virtual time.

    python3 -I pusher_replay.py <push-gpu-fans.py> <trace.jsonl> <out.jsonl>

The pusher is imported unchanged; only its clock, its nvidia-smi subprocess, its HTTP POST
and its log are replaced:

- time.monotonic / time.sleep run on a virtual clock (trace times are seconds from 0);
- nvidia-smi's stdout yields the trace's `line`s at their times, in lockstep with the clock:
  every line with t <= the end of a sleep is consumed (and processed by the pusher's reader
  thread) before the sleep returns;
- post(entity, value) records {"t", "entity", "value"} instead of sending, and raises
  URLError inside any `{"fail": [t0, t1]}` window given in the trace;
- the stream watchdog thread is not started (the trace has no hung process to kill).

Trace lines: {"t": 1.25, "line": "00000000:01:00.0, 40, 38, 120.5, 250.00"}, optionally one
{"env": {"FAN_MAP": "..."}} first, and {"fail": [t0, t1]} windows. Nothing is ever sent:
FAN_AUTH/FAN_HOST are dummies and the network functions are replaced before main() runs.
"""
import importlib.util
import json
import os
import queue
import sys
import tempfile
import threading
import urllib.error

pusher_path, trace_path, out_path = sys.argv[1:4]
events = [json.loads(l) for l in open(trace_path, encoding="utf-8") if l.strip()]
env = {"FAN_AUTH": "replay:replay", "FAN_HOST": "replay.invalid", "FAN_MAP": "01:00.0 21:00.0 E1:00.0"}
for e in events:
    env.update(e.get("env", {}))
lines = [(e["t"], e["line"]) for e in events if "line" in e]
fails = [tuple(e["fail"]) for e in events if "fail" in e]
empty = tempfile.NamedTemporaryFile("w", delete=False, suffix=".env")
empty.close()
os.environ.update(env)
os.environ["FAN_ENV"] = empty.name  # an empty env file: nothing is read from the host

spec = importlib.util.spec_from_file_location("pusher", pusher_path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

clock = [0.0]
END = (max(t for t, _ in lines) if lines else 0.0) + 15.0
feed = queue.Queue()
consumed = threading.Semaphore(0)
reader_waiting = threading.Event()


class Done(Exception):
    pass


class FakeStdout:
    def __iter__(self):
        return self

    def __next__(self):
        consumed.release()          # the previous line (if any) is fully processed
        return feed.get()           # blocks forever after the trace ends


class FakeProc:
    stdout = FakeStdout()

    def kill(self):
        pass

    def wait(self):
        return 0


class FakeSubprocess:
    PIPE = DEVNULL = None

    @staticmethod
    def Popen(*a, **k):
        return FakeProc()


class FakeTime:
    @staticmethod
    def monotonic():
        return clock[0]

    @staticmethod
    def sleep(dt):
        if threading.current_thread() is not threading.main_thread():
            raise RuntimeError("only the main loop may sleep in the replay")
        target = clock[0] + dt
        while next_line[0] < len(lines) and lines[next_line[0]][0] <= target:
            t, line = lines[next_line[0]]
            next_line[0] += 1
            clock[0] = max(clock[0], t)
            feed.put(line + "\n")
            consumed.acquire()      # wait until the reader asks for the next line
        clock[0] = target
        if clock[0] > END:
            raise Done()

    time = staticmethod(__import__("time").time)
    strftime = staticmethod(__import__("time").strftime)
    localtime = staticmethod(__import__("time").localtime)


class FakeThreading:
    Lock = threading.Lock
    Event = threading.Event

    class Thread:
        def __init__(self, target=None, daemon=None, **k):
            self.target = target

        def start(self):
            if self.target.__name__ == "watchdog":
                return
            t = threading.Thread(target=self.target, daemon=True)
            t.start()
            consumed.acquire()      # the reader is at its first next()


next_line = [0]
out = []


def fake_post(entity, value):
    t = clock[0]
    if any(a <= t <= b for a, b in fails):
        raise urllib.error.URLError("replay: injected failure")
    out.append({"t": round(t, 3), "entity": entity, "value": value})


log = []
mod.time = FakeTime
mod.subprocess = FakeSubprocess
mod.threading = FakeThreading
mod.post = fake_post
mod.log = lambda m: log.append((clock[0], m))

try:
    mod.main()
except Done:
    pass

with open(out_path, "w", encoding="utf-8") as fh:
    for o in out:
        fh.write(json.dumps(o) + "\n")
os.unlink(empty.name)
print(f"{len(lines)} samples -> {len(out)} posts, {len(log)} log lines", file=sys.stderr)
