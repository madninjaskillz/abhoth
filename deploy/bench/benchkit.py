#!/usr/bin/env python3
"""Bench task kit driver for the web UI's bench runner (src/OpenOrca.WebUi/Bench).

  python3 -I benchkit.py <kit> list                      -> [{"name", "timeout", "tier"}]
  python3 -I benchkit.py <kit> setup <task> <ws>         -> {"prompt", "timeout"}  (fresh workspace)
  python3 -I benchkit.py <kit> check <task> <ws> <dir>   -> {"pass", "reason", "tests_passed", "tests_total"}

<kit> holds the task data (copied from the bench scripts, never edited in place):
  benchlib.py, tasks_t3.py, tasks_fanout.py, multi/fanout8/     fanout8 (8 small Python modules)
  fanout2/ (ezkit.py, tasks_fanout2.py, template, hidden, packages)  fanout-ez and fanout-ez-<component>
  realtasks/ (realtasks.py, baseline/) + REALTASKS_REPO (an openorca git clone)  agents-per-copy, …

The repo's own bench tasks (bench/tasks.py and its tiers: kvstore, t3-cli-feature, multi-gems, …)
are offered too when the bench folder of an openorca checkout is at <kit>/bench (a copy or a mount)
or named by OPENORCA_BENCH_TASKS. It is used as it is: seeds, hidden tests and reference solutions
are read from it, nothing is written there. A kit task of the same name wins.

The last stdout line is the JSON result; anything a checker prints goes to <dir>/kit.log.
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import sys
from pathlib import Path


def load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


# The modules of the repo's bench folder. The kit carries copies of two of them (benchlib,
# tasks_t3) for its own tasks, and each copy finds its data next to itself, so the two sets must
# not see each other.
REPO_MODULES = ("benchlib", "tasks", "tasks_hard", "tasks_t3", "tasks_multi", "t3_editgen")


def repo_bench_dir(kit: Path) -> Path | None:
    """The bench folder of an openorca checkout: OPENORCA_BENCH_TASKS, else <kit>/bench."""
    named = os.environ.get("OPENORCA_BENCH_TASKS")
    for d in ([Path(named)] if named else []) + [kit / "bench"]:
        if (d / "tasks.py").is_file():
            return d.resolve()
    return None


def repo_tasks(bench: Path) -> dict:
    """bench/tasks.py's all_tasks(), imported apart from the kit's modules of the same names."""
    kept = {n: sys.modules.pop(n) for n in REPO_MODULES if n in sys.modules}
    sys.path.insert(0, str(bench))
    try:
        return load(bench / "tasks.py", "tasks").all_tasks()
    finally:
        sys.path.remove(str(bench))
        for n in REPO_MODULES:
            sys.modules.pop(n, None)
        sys.modules.update(kept)


def tasks(kit: Path) -> dict:
    sys.path.insert(0, str(kit))
    found = {}
    errors = []
    bench = repo_bench_dir(kit)
    # A kit that only serves the repo's bench tasks has no fanout families: nothing to report.
    if bench is None or (kit / "tasks_fanout.py").exists():
        try:
            tf = load(kit / "tasks_fanout.py", "tasks_fanout")
            found.update({t.name: t for t in tf.FANOUT})
        except Exception as e:  # noqa: BLE001 - a missing task family must not hide the others
            errors.append(f"fanout8: {e}")
    if bench is None or (kit / "fanout2" / "tasks_fanout2.py").exists():
        try:
            tf2 = load(kit / "fanout2" / "tasks_fanout2.py", "tasks_fanout2")
            found.update({t.name: t for t in tf2.FANOUT2})
        except Exception as e:  # noqa: BLE001
            errors.append(f"fanout-ez: {e}")
    if (kit / "realtasks" / "realtasks.py").exists() and os.environ.get("REALTASKS_REPO"):
        try:
            rt = load(kit / "realtasks" / "realtasks.py", "realtasks")
            found.update(rt.TASKS)
        except Exception as e:  # noqa: BLE001
            errors.append(f"realtasks: {e}")
    if bench is not None:
        try:
            for name, task in repo_tasks(bench).items():
                found.setdefault(name, task)
        except Exception as e:  # noqa: BLE001
            errors.append(f"bench tasks ({bench}): {e}")
    for e in errors:
        print(f"[benchkit] {e}", file=sys.stderr)
    return found


def counts(task: str, check_dir: Path, ok: bool) -> tuple[int, int]:
    """(tests passed, tests total) from the checker's score.json when it has them; else 1/1 or 0/1."""
    f = check_dir / "score.json"
    try:
        s = json.loads(f.read_text(encoding="utf-8"))
        if "components" in s:  # fanout-ez
            p = sum(c.get("passed", 0) for c in s["components"].values())
            t = sum(c.get("total", 0) for c in s["components"].values())
            if t:
                return p, t
        if "tests" in s and "failed" in s:  # realtasks
            return s["tests"] - s["failed"], s["tests"]
    except (OSError, ValueError, TypeError, AttributeError):
        pass
    return (1 if ok else 0), 1


def main(argv: list[str]) -> int:
    kit, cmd, *rest = argv
    kit = Path(kit).resolve()
    all_tasks = tasks(kit)
    if cmd == "list":
        print(json.dumps([{"name": n, "timeout": getattr(t, "timeout", 900), "tier": getattr(t, "tier", None)} for n, t in sorted(all_tasks.items())]))
        return 0
    name = rest[0]
    if name not in all_tasks:
        print(json.dumps({"error": f"unknown task {name}"}))
        return 2
    task = all_tasks[name]
    if cmd == "setup":
        ws = Path(rest[1])
        task.materialize(ws)
        print(json.dumps({"prompt": task.prompt, "timeout": getattr(task, "timeout", 900)}))
        return 0
    if cmd == "check":
        ws, out = Path(rest[1]), Path(rest[2])
        out.mkdir(parents=True, exist_ok=True)
        log = io.StringIO()
        try:
            with contextlib.redirect_stdout(log), contextlib.redirect_stderr(log):
                ok, reason = task.check(ws, out)
        except Exception as e:  # noqa: BLE001
            ok, reason = False, f"checker error: {e}"
        (out / "kit.log").write_text(log.getvalue(), encoding="utf-8")
        p, t = counts(name, out, ok)
        print(json.dumps({"pass": bool(ok), "reason": "ok" if ok else str(reason)[:500], "tests_passed": p, "tests_total": t}))
        return 0
    print(json.dumps({"error": f"unknown command {cmd}"}))
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
