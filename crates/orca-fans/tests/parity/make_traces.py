#!/usr/bin/env python3
"""Builds the parity traces in ../fixtures/parity/ (deterministic: fixed seeds).

    python3 -I make_traces.py <out_dir> [recording.jsonl]

- heat_ramp.jsonl: idle → load → HBM rising through the 52-64 °C load gate to 80 °C (over
  max_temp territory) → cool-down; card D has no HBM sensor; one load spike above 150 %.
- faults.jsonl: a card missing 5 s and 15 s, the whole stream silent 12 s, power "N/A",
  HBM going "N/A" (core fallback), and two POST failure windows.
- fanmap.jsonl: FAN_MAP "01:00.0+21:00.0 max E1:00.0" (strict pair, max of all) with a
  card dropping out of the pair.
- recorded.jsonl: the nvidia-smi lines of a real recording (only `smi` lines are kept; bus
  ids only, no UUIDs or host names), if a recording is given.
"""
import json
import math
import os
import random
import sys

BUSES = [("00000000:01:00.0", 250.0), ("00000000:21:00.0", 350.0), ("00000000:44:00.0", 120.0), ("00000000:E1:00.0", 300.0)]


def cycle_times(t, rng):
    """nvidia-smi prints one line per GPU ~15-20 ms apart, every ~500 ms."""
    out, x = [], t
    for _ in BUSES:
        out.append(round(x, 3))
        x += 0.012 + rng.random() * 0.01
    return out


def fmt(core, hbm, p, lim):
    h = "N/A" if hbm is None else str(int(round(hbm)))
    ps = "N/A" if p is None else f"{p:.2f}"
    return f"{int(round(core))}, {h}, {ps}, {lim:.2f}"


def heat_ramp(rng):
    ev, t = [], 0.05
    while t < 420:
        for i, ((bus, lim), tt) in enumerate(zip(BUSES, cycle_times(t, rng))):
            phase = tt
            if phase < 60:
                lvl = 0.0
            elif phase < 240:
                lvl = min(1.0, (phase - 60) / 120)
            elif phase < 300:
                lvl = 1.0
            else:
                lvl = max(0.0, 1 - (phase - 300) / 60)
            base = [27, 26, 30, 25][i]
            span = [55, 58, 25, 50][i]
            hbm = base + span * lvl + rng.gauss(0, 0.6)
            core = hbm - 3 + rng.gauss(0, 0.6)
            load = (0.08 + 0.9 * lvl) * lim * (1 + rng.gauss(0, 0.12))
            if i == 1 and 200 <= phase < 203:
                load = 1.62 * lim      # spike above the ESP's 150 % range
            ev.append({"t": tt, "line": f"{bus}, " + fmt(core, None if i == 2 else hbm, max(load, 5.0), lim)})
        t += 0.5 + rng.gauss(0, 0.004)
    return ev


def faults(rng):
    ev, t = [], 0.05
    ev.append({"fail": [100.0, 104.0]})
    ev.append({"fail": [230.0, 231.0]})
    while t < 330:
        silent = 150 <= t < 162                      # the whole stream hangs 12 s
        for i, ((bus, lim), tt) in enumerate(zip(BUSES, cycle_times(t, rng))):
            if silent:
                continue
            if i == 1 and (40 <= tt < 45 or 80 <= tt < 95):
                continue                             # card A missing 5 s, then 15 s
            hbm = 50 + 18 * math.sin(tt / 25) + rng.gauss(0, 0.5) + 4 * i
            core = hbm - 4
            p = (0.5 + 0.45 * math.sin(tt / 9)) * lim
            if i == 0 and 180 <= tt < 200:
                p = None                             # power N/A
            h = hbm
            if i == 3 and 250 <= tt < 270:
                h = None                             # HBM N/A → core fallback
            if i == 2:
                h = None
            ev.append({"t": tt, "line": f"{bus}, " + fmt(core, h, p, lim)})
        t += 0.5 + rng.gauss(0, 0.004)
    return ev


def fanmap(rng):
    ev = [{"env": {"FAN_MAP": "01:00.0+21:00.0 max E1:00.0"}}]
    t = 0.05
    while t < 240:
        for i, ((bus, lim), tt) in enumerate(zip(BUSES, cycle_times(t, rng))):
            if i == 0 and 120 <= tt < 126:
                continue                             # B drops out of the strict pair
            hbm = 45 + 25 * (0.5 + 0.5 * math.sin(tt / 30 + i)) + rng.gauss(0, 0.4)
            p = (0.3 + 0.6 * (0.5 + 0.5 * math.sin(tt / 7 + i))) * lim
            ev.append({"t": tt, "line": f"{bus}, " + fmt(hbm - 2, None if i == 2 else hbm, p, lim)})
        t += 0.5 + rng.gauss(0, 0.004)
    return ev


def recorded(path):
    ev = []
    for l in open(path, encoding="utf-8"):
        e = json.loads(l)
        if e.get("src") == "smi":
            ev.append({"t": e["t"], "line": e["line"]})
    return ev


def write(out_dir, name, ev):
    with open(os.path.join(out_dir, name), "w", encoding="utf-8", newline="\n") as fh:
        for e in ev:
            fh.write(json.dumps(e, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    write(out, "heat_ramp.jsonl", heat_ramp(random.Random(1)))
    write(out, "faults.jsonl", faults(random.Random(2)))
    write(out, "fanmap.jsonl", fanmap(random.Random(3)))
    if len(sys.argv) > 2:
        write(out, "recorded.jsonl", recorded(sys.argv[2]))
