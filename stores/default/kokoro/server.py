"""A small OpenAI-speech server for Kokoro (abhoth engine package `kokoro`, pip method).

POST /v1/audio/speech {"input", "voice", "speed"} -> audio/wav
GET  /health, GET /v1/models, GET /v1/audio/voices
Standard library HTTP server; one synthesis at a time (the router queues).
"""
import argparse, io, json, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import soundfile as sf
from kokoro import KPipeline

ap = argparse.ArgumentParser()
ap.add_argument("--host", default="127.0.0.1")
ap.add_argument("--port", type=int, default=8880)
ap.add_argument("--voice", default="af_bella")
ap.add_argument("--lang", default="a")
args = ap.parse_args()

pipe = KPipeline(lang_code=args.lang)
lock = threading.Lock()
VOICES = ["af_bella", "af_heart", "af_nicole", "am_michael", "am_adam", "bf_emma", "bm_fable", "bm_george"]


class H(BaseHTTPRequestHandler):
    def _json(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path == "/health":
            return self._json(200, {"ok": True, "engine": "kokoro"})
        if self.path == "/v1/models":
            return self._json(200, {"object": "list", "data": [{"id": "kokoro", "object": "model"}]})
        if self.path == "/v1/audio/voices":
            return self._json(200, {"voices": VOICES})
        self._json(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/v1/audio/speech":
            return self._json(404, {"error": "not found"})
        n = int(self.headers.get("Content-Length") or 0)
        if n > 1_000_000:
            return self._json(413, {"error": "too large"})
        try:
            req = json.loads(self.rfile.read(n) or b"{}")
        except ValueError:
            return self._json(400, {"error": "the body must be JSON"})
        text = str(req.get("input") or "").strip()
        if not text:
            return self._json(400, {"error": "input is empty"})
        voice = str(req.get("voice") or args.voice)
        speed = float(req.get("speed") or 1.0)
        with lock:
            chunks = [a for _, _, a in pipe(text, voice=voice, speed=speed)]
        audio = np.concatenate([np.asarray(c) for c in chunks]) if chunks else np.zeros(1)
        buf = io.BytesIO()
        sf.write(buf, audio, 24000, format="WAV")
        b = buf.getvalue()
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def log_message(self, fmt, *a):
        pass


ThreadingHTTPServer((args.host, args.port), H).serve_forever()
