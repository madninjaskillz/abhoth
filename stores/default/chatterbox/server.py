"""A small OpenAI-speech server for Chatterbox (abhoth engine package `chatterbox`, pip method).

POST /v1/audio/speech {"input", "voice"} -> audio/wav; GET /health, /voices, /v1/models.
`voice` names a reference clip <voice>.wav in --voices (empty: the model's own voice).
"""
import argparse, io, json, os, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import torch
import torchaudio as ta
from chatterbox.tts import ChatterboxTTS

ap = argparse.ArgumentParser()
ap.add_argument("--host", default="127.0.0.1")
ap.add_argument("--port", type=int, default=10204)
ap.add_argument("--voices", default="")
ap.add_argument("--voice", default="")
args = ap.parse_args()

device = "cuda" if torch.cuda.is_available() else "cpu"
model = ChatterboxTTS.from_pretrained(device=device)
lock = threading.Lock()


def voices():
    d = args.voices
    if not d or not os.path.isdir(d):
        return []
    return sorted(f[:-4] for f in os.listdir(d) if f.endswith(".wav"))


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
            return self._json(200, {"ok": True, "engine": "chatterbox", "device": device})
        if self.path == "/voices":
            return self._json(200, {"voices": voices()})
        if self.path == "/v1/models":
            return self._json(200, {"object": "list", "data": [{"id": "chatterbox", "object": "model"}]})
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
        v = str(req.get("voice") or args.voice)
        prompt = os.path.join(args.voices, v + ".wav") if v and v in voices() else None
        with lock:
            wav = model.generate(text, audio_prompt_path=prompt)
        buf = io.BytesIO()
        ta.save(buf, wav, model.sr, format="wav")
        b = buf.getvalue()
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def log_message(self, fmt, *a):
        pass


ThreadingHTTPServer((args.host, args.port), H).serve_forever()
