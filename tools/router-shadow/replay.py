#!/usr/bin/env python3
"""Shadow replay: the same request shapes through LiteLLM and through `abhoth router`, compared.

local  (default) both gateways run on this machine in front of echo engines that record what reaches
       them: the live LiteLLM config (tests/fixtures/litellm-config.yaml, layoutctl's output) with its
       qfn-lb / relay bases moved to the echoes, effort_hook.py as on DeepThought, and the router's
       config imported from the same files. Compares the UPSTREAM request bodies (what the engine
       gets) and the client-visible responses (status, served-model header, JSON shape).

    python replay.py --litellm-bin <venv>/Scripts/litellm --router-bin <target>/debug/abhoth-router \
                     --hook <effort_hook.py> [--report out.json]

live   read-only GET comparison of two running gateways (/v1/models, /v1/model/info, /health/liveliness);
       with --chat also small chats on the live GENERIC aliases only (needs the user's go-ahead).
       Never calls /health on LiteLLM (it test-calls every deployment, relay ones included, which
       switches GPUs) and never names an on-demand model.

    python replay.py live --a http://gpu-host:8010 --b http://gpu-host:8020 [--chat]
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from shapes import SHAPES  # noqa: E402

FIXTURES = os.path.join(HERE, "..", "..", "crates", "orca-router", "tests", "fixtures")

SEEN = []
LOCK = threading.Lock()


def make_echo(name):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def _json(self, code, obj):
            data = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path.rstrip("/").endswith("/models"):
                return self._json(200, {"object": "list", "data": [{"id": "inkling-2-mini", "object": "model"}]})
            return self._json(200, {"status": "ok"})

        def do_POST(self):
            n = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(n)
            try:
                body = json.loads(raw)
            except ValueError:
                body = None
            with LOCK:
                SEEN.append({"server": name, "path": self.path, "body": body,
                             "auth": "yes" if self.headers.get("Authorization") else "no"})
            model = (body or {}).get("model", "?")
            if (body or {}).get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                events = [
                    {"id": "e1", "object": "chat.completion.chunk", "created": 1, "model": model,
                     "choices": [{"index": 0, "delta": {"role": "assistant", "content": "O"}, "finish_reason": None}]},
                    {"id": "e1", "object": "chat.completion.chunk", "created": 1, "model": model,
                     "choices": [{"index": 0, "delta": {"content": "K"}, "finish_reason": "stop"}]},
                ]
                if ((body or {}).get("stream_options") or {}).get("include_usage"):
                    events.append({"id": "e1", "object": "chat.completion.chunk", "created": 1, "model": model,
                                   "choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2,
                                                            "total_tokens": 7}})
                for ev in events:
                    self._chunk(("data: " + json.dumps(ev) + "\n\n").encode())
                self._chunk(b"data: [DONE]\n\n")
                self._chunk(b"")
                return
            self._json(200, {"id": "e1", "object": "chat.completion", "created": 1, "model": model,
                             "choices": [{"index": 0, "message": {"role": "assistant", "content": "OK"},
                                          "finish_reason": "stop"}],
                             "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}})

        def _chunk(self, b):
            self.wfile.write(f"{len(b):x}\r\n".encode() + b + b"\r\n")
            self.wfile.flush()
    return H


def start_echo(name):
    srv = ThreadingHTTPServer(("127.0.0.1", 0), make_echo(name))
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv.server_address[1]


def wait_up(url, timeout=120):
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:  # noqa: BLE001
            time.sleep(0.5)
    return False


def call(base, body, headers):
    req = urllib.request.Request(base + "/v1/chat/completions", json.dumps(body).encode(),
                                 {"Content-Type": "application/json", **headers})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, dict(r.headers), r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode("utf-8", "replace")


def diff(a, b, path=""):
    """Differences between two JSON values, as strings."""
    out = []
    if isinstance(a, dict) and isinstance(b, dict):
        for k in a:
            if k not in b:
                out.append(f"{path}/{k}: only LiteLLM sends {json.dumps(a[k])[:80]}")
            else:
                out += diff(a[k], b[k], f"{path}/{k}")
        for k in b:
            if k not in a:
                out.append(f"{path}/{k}: only the router sends {json.dumps(b[k])[:80]}")
    elif a != b:
        out.append(f"{path}: LiteLLM {json.dumps(a)[:80]} vs router {json.dumps(b)[:80]}")
    return out


def sse_shape(text):
    """The JSON events of an SSE body, keys only per event (values differ in ids/created)."""
    ev = []
    for line in text.splitlines():
        if line.startswith("data: ") and line[6:].strip() != "[DONE]":
            try:
                d = json.loads(line[6:])
                ev.append(sorted(d.keys()))
            except ValueError:
                ev.append("non-json")
    return ev


def local(args):
    work = tempfile.mkdtemp(prefix="router-shadow-")
    lb_port, a_port, b_port, relay_port = (start_echo(n) for n in ("qfn-lb", "A", "B", "relay"))
    with open(os.path.join(FIXTURES, "litellm-config.yaml")) as f:
        orig = f.read().replace("http://gpu-host:8098", f"http://127.0.0.1:{relay_port}")
    # LiteLLM talks to qfn-lb (an echo here); the router reads the same file and the nginx.conf
    conf = orig.replace("http://gpu-host:8092", f"http://127.0.0.1:{lb_port}")
    with open(os.path.join(FIXTURES, "nginx.conf")) as f:
        ngx = f.read().replace("gpu-host:8094", f"127.0.0.1:{a_port}").replace("gpu-host:8096",
                                                                                     f"127.0.0.1:{b_port}")
    cfg = os.path.join(work, "config.yaml")
    with open(cfg, "w") as f:
        f.write(conf)
    with open(os.path.join(work, "nginx.conf"), "w") as f:
        f.write(ngx)
    shutil.copy(args.hook, os.path.join(work, "effort_hook.py"))
    with open(os.path.join(work, "key"), "w") as f:
        f.write("test-key-placeholder\n")
    rj = os.path.join(work, "router.json")
    router_src = os.path.join(work, "config.router-src.yaml")
    with open(router_src, "w") as f:
        f.write(orig)
    subprocess.run([args.router_bin, "import-litellm", "--litellm", router_src, "--lb", os.path.join(work, "nginx.conf"),
                    "--key-file", os.path.join(work, "key"), "--out", rj], check=True)
    ll_port, rt_port = args.litellm_port, args.router_port
    env = dict(os.environ, GATEWAY_CONFIG=cfg, LITELLM_LOG="ERROR", PYTHONIOENCODING="utf-8")
    ll = subprocess.Popen([args.litellm_bin, "--config", cfg, "--port", str(ll_port), "--host", "127.0.0.1"],
                          cwd=work, env=env, stdout=open(os.path.join(work, "litellm.log"), "w"),
                          stderr=subprocess.STDOUT)
    rt = subprocess.Popen([args.router_bin, "--config", rj, "--listen", f"127.0.0.1:{rt_port}"],
                          stdout=open(os.path.join(work, "router.log"), "w"), stderr=subprocess.STDOUT)
    try:
        if not wait_up(f"http://127.0.0.1:{ll_port}/health/liveliness", 180):
            sys.exit(f"LiteLLM did not start; see {work}/litellm.log")
        if not wait_up(f"http://127.0.0.1:{rt_port}/health/liveliness", 30):
            sys.exit(f"router did not start; see {work}/router.log")
        report = {"work": work, "shapes": []}
        bases = {"litellm": f"http://127.0.0.1:{ll_port}", "router": f"http://127.0.0.1:{rt_port}"}
        for name, client, body, headers in SHAPES:
            res = {}
            for gw, base in bases.items():
                with LOCK:
                    SEEN.clear()
                st, hd, text = call(base, body, headers)
                time.sleep(0.1)
                with LOCK:
                    up = list(SEEN)
                hd = {k.lower(): v for k, v in hd.items()}
                res[gw] = {"status": st, "served": hd.get("x-litellm-model-name"), "upstream": up,
                           "ctype": hd.get("content-type", "").split(";")[0],
                           "shape": sse_shape(text) if body.get("stream") else
                           (sorted(json.loads(text).keys()) if text.startswith("{") else text[:100]),
                           "text": text[:600]}
            a, b = res["litellm"], res["router"]
            notes = []
            if a["status"] != b["status"]:
                notes.append(f"status: LiteLLM {a['status']} vs router {b['status']}")
            if len(a["upstream"]) != len(b["upstream"]):
                notes.append(f"upstream calls: LiteLLM {len(a['upstream'])} vs router {len(b['upstream'])}")
            if a["upstream"] and b["upstream"]:
                notes += ["body " + d for d in diff(a["upstream"][-1]["body"], b["upstream"][-1]["body"])]
            if a["served"] != b["served"]:
                notes.append(f"x-litellm-model-name: LiteLLM {a['served']} vs router {b['served']}")
            if a["shape"] != b["shape"]:
                notes.append(f"response shape: LiteLLM {a['shape']} vs router {b['shape']}")
            report["shapes"].append({"name": name, "client": client, "notes": notes, "litellm": a, "router": b})
            print(f"{'SAME' if not notes else 'DIFF'}  {name:24s} " + ("; ".join(notes) if notes else ""))
        # read-only endpoints
        for p in ("/v1/models", "/v1/model/info"):
            ja = json.load(urllib.request.urlopen(bases["litellm"] + p))
            jb = json.load(urllib.request.urlopen(bases["router"] + p))
            if p == "/v1/models":
                d = diff(ja, jb)
            else:
                d = []
                for x, y in zip(ja["data"], jb["data"]):
                    xi = {k: v for k, v in x["model_info"].items() if k in y["model_info"] and k != "id"}
                    yi = {k: v for k, v in y["model_info"].items() if k != "id"}
                    d += [f"{x['model_name']}: {e}" for e in diff(xi, yi)]
                    if x["model_name"] != y["model_name"]:
                        d.append(f"order: {x['model_name']} vs {y['model_name']}")
            print(f"{'SAME' if not d else 'DIFF'}  GET {p} " + "; ".join(d[:10]))
            report[p] = d
        if args.report:
            with open(args.report, "w") as f:
                json.dump(report, f, indent=1)
    finally:
        for p in (ll, rt):
            p.terminate()
        for p in (ll, rt):
            try:
                p.wait(10)
            except subprocess.TimeoutExpired:
                p.kill()


def live(args):
    """Read-only comparison of two running gateways; --chat adds tiny chats on generic aliases."""
    bad = 0
    for p in ("/v1/models", "/v1/model/info", "/health/liveliness"):
        ja = json.load(urllib.request.urlopen(args.a + p, timeout=10))
        jb = json.load(urllib.request.urlopen(args.b + p, timeout=10))
        if p == "/v1/model/info":
            d = []
            for x, y in zip(ja["data"], jb["data"]):
                xi = {k: v for k, v in x["model_info"].items() if k in y["model_info"] and k != "id"}
                yi = {k: v for k, v in y["model_info"].items() if k != "id"}
                d += [f"{x['model_name']}: {e}" for e in diff(xi, yi)]
        else:
            d = diff(ja, jb)
        bad += bool(d)
        print(f"{'SAME' if not d else 'DIFF'}  GET {p} " + "; ".join(d[:10]))
    if args.chat:
        models = json.load(urllib.request.urlopen(args.b + "/v1/model/info", timeout=10))["data"]
        relay = {m["model_name"] for m in models if m["model_info"].get("live") is False}
        for name, client, body, headers in SHAPES:
            if body["model"] in relay or not re.match(r"^(qwen3\.[0-9].*|jarvis-brain|inkling-2-mini)$", body["model"]):
                print(f"SKIP  {name} ({body['model']} is not a live alias)")
                continue
            sa, _, ta = call(args.a, body, headers)
            sb, hb, tb = call(args.b, body, headers)
            note = "" if sa == sb else f"status {sa} vs {sb}"
            print(f"{'SAME' if not note else 'DIFF'}  {name:24s} {note} router served by "
                  f"{hb.get('x-abhoth-card') or hb.get('X-Abhoth-Card')}")
            time.sleep(args.pause)
    return bad


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", nargs="?", default="local", choices=["local", "live"])
    ap.add_argument("--litellm-bin")
    ap.add_argument("--router-bin")
    ap.add_argument("--hook")
    ap.add_argument("--litellm-port", type=int, default=18010)
    ap.add_argument("--router-port", type=int, default=18020)
    ap.add_argument("--report")
    ap.add_argument("--a", help="live: LiteLLM base URL")
    ap.add_argument("--b", help="live: router base URL")
    ap.add_argument("--chat", action="store_true")
    ap.add_argument("--pause", type=float, default=2.0)
    args = ap.parse_args()
    if args.mode == "local":
        local(args)
    else:
        sys.exit(1 if live(args) else 0)


if __name__ == "__main__":
    main()
