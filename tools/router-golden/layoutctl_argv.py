#!/usr/bin/env python3
"""Render parity harness for the router importer (ROUTER-CONFIG.md 13.1).

Imports a READ-ONLY copy of layoutctl.py with every side effect stubbed (no docker, no files
written, no network), then records, for every layout and card, the docker argv layoutctl would
run (`start_ninfer` / `start_llamacpp`) or the start command (`start_strata`), plus
`restart_set` for every (from, to) pair and `slot_names` per layout. The key is replaced by
`***`. Output: one JSON document on stdout.

    python3 -I layoutctl_argv.py /copy/of/layouts > layoutctl-golden.json

The router's golden test (crates/orca-router/tests/import_golden.rs) imports the same copy and
compares.
"""
import importlib.util
import json
import os
import sys
import types


def main(layouts_dir):
    sys.modules.setdefault("fcntl", types.ModuleType("fcntl"))  # not used by what we call
    import zoneinfo
    real_zi = zoneinfo.ZoneInfo

    def zi(key):  # hosts without tzdata (python -I on Windows): time zones are not compared
        try:
            return real_zi(key)
        except zoneinfo.ZoneInfoNotFoundError:
            from datetime import timezone
            return timezone.utc
    zoneinfo.ZoneInfo = zi
    sys.path.insert(0, layouts_dir)
    spec = importlib.util.spec_from_file_location("layoutctl", os.path.join(layouts_dir, "layoutctl.py"))
    lc = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(lc)

    recorded = []

    class Done:
        returncode, stdout, stderr = 0, "", ""

    def fake_run(args, **kw):
        recorded.append(list(args))
        return Done()

    def fake_sh(*args, check=True, timeout=600):
        recorded.append(list(args))
        return Done()

    lc.subprocess.run = fake_run
    lc.sh = fake_sh
    lc.remove_container = lambda name, timeout=120: None
    lc.api_key = lambda: "***"
    lc.rendered_compose = lambda: "$RENDERED_COMPOSE"
    lc.strata_slots = lambda card: "?"
    lc.os.access = lambda p, m: True
    real_isfile = os.path.isfile
    lc.os.path.isfile = lambda p: True if p.startswith("/") else real_isfile(p)
    lc.os.makedirs = lambda *a, **k: None

    class Job:
        def step(self, msg):
            pass

    cat = lc.catalog()
    out = {"layouts": {}, "restart_set": {}, "slot_names": {}}
    names = sorted(cat.get("layouts") or {})
    specs = {}
    for name in names:
        try:
            spec_ = lc.layout_spec(cat, name)
        except Exception as e:  # noqa: BLE001
            out["layouts"][name] = {"error": str(e)}
            continue
        specs[name] = spec_
        cards = {}
        for card in lc.CARDS:
            sc = spec_[card]
            if sc["engine"] == "off":
                cards[card] = None
                continue
            recorded.clear()
            try:
                lc.start_engine(card, sc, cat, Job())
                cards[card] = {"engine": sc["engine"], "argv": [a for a in recorded if a and a[0] == "docker"][-1],
                               "label": json.dumps(lc.norm_card(sc), sort_keys=True)}
            except Exception as e:  # noqa: BLE001
                cards[card] = {"engine": sc["engine"], "error": str(e)}
        out["layouts"][name] = cards
        out["slot_names"][name] = lc.slot_names(spec_)
    for a in specs:
        for b in specs:
            out["restart_set"][f"{a}>{b}"] = lc.restart_set(specs[a], specs[b])
    json.dump(out, sys.stdout, indent=1, sort_keys=True)


if __name__ == "__main__":
    main(sys.argv[1])
