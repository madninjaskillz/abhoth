"""Request shapes the gateway's clients send (ROUTER.md 1.2), reconstructed from the client code and
LiteLLM's logs. No keys, no real conversations: small prompts, max_tokens small.

Each shape: (name, client, body, extra headers). `model` is a live generic alias or pinned name only;
replaying never names an on-demand (relay) model.
"""

TINY_PNG = ("data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")

SYS = {"role": "system", "content": "You are terse."}
HI = {"role": "user", "content": "Reply with the single word OK."}

TOOLS = [{"type": "function", "function": {"name": "read_file", "description": "Read a file",
          "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}]

SHAPES = [
    # layoutctl gateway_test (first alias, non-streaming, max_tokens 64)
    ("layoutctl-test", "layoutctl", {"model": "qwen3.8-flash-next", "max_tokens": 64, "stream": False,
                                     "messages": [HI]}, {}),
    # video pipelines research_local.py: pinned card A, json_object, then the thinking-off retry
    ("pipeline-json", "pipelines", {"model": "qwen3.8-flash-next-a", "messages": [SYS, HI], "temperature": 0.2,
                                    "max_tokens": 64, "response_format": {"type": "json_object"}}, {}),
    ("pipeline-nothink", "pipelines", {"model": "qwen3.8-flash-next-a", "messages": [SYS, HI], "temperature": 0.2,
                                       "max_tokens": 64, "chat_template_kwargs": {"enable_thinking": False}}, {}),
    # Jarvis llm.py: always thinking off, json_object, non-streaming and streaming, vision turn
    ("jarvis", "jarvis", {"model": "jarvis-brain", "messages": [SYS, HI], "temperature": 0.6, "max_tokens": 64,
                          "response_format": {"type": "json_object"},
                          "chat_template_kwargs": {"enable_thinking": False}}, {}),
    ("jarvis-stream", "jarvis", {"model": "jarvis-brain", "messages": [SYS, HI], "temperature": 0.6, "max_tokens": 64,
                                 "stream": True, "chat_template_kwargs": {"enable_thinking": False}}, {}),
    ("jarvis-vision", "jarvis", {"model": "jarvis-brain", "max_tokens": 32,
                                 "chat_template_kwargs": {"enable_thinking": False},
                                 "messages": [{"role": "user", "content": [
                                     {"type": "text", "text": "What colour is this pixel? One word."},
                                     {"type": "image_url", "image_url": {"url": TINY_PNG}}]}]}, {}),
    # OpenOrca / Abhoth: streaming, tools, effort, preserve_thinking, stream_options
    ("orca-tools", "openorca", {"model": "qwen3.8-27b", "messages": [SYS, HI], "stream": True, "max_tokens": 64,
                                "tools": TOOLS, "tool_choice": "auto", "temperature": 0.6, "top_p": 0.95,
                                "reasoning_effort": "low", "stream_options": {"include_usage": True},
                                "chat_template_kwargs": {"preserve_thinking": True}},
     {"authorization": "Bearer not-a-real-key"}),
    ("orca-effort-high", "openorca", {"model": "qwen3.8-27b-coder", "messages": [HI], "max_tokens": 32,
                                      "reasoning_effort": "high"}, {}),
    ("orca-effort-max", "openorca", {"model": "qwen3.8-27b-coder", "messages": [HI], "max_tokens": 32,
                                     "reasoning_effort": "max"}, {}),
    ("orca-effort-minimal", "openorca", {"model": "qwen3.8-27b-coder", "messages": [HI], "max_tokens": 32,
                                         "reasoning_effort": "minimal"}, {}),
    ("orca-off-with-effort", "openorca", {"model": "qwen3.8-27b", "messages": [HI], "max_tokens": 32,
                                          "reasoning_effort": "low",
                                          "chat_template_kwargs": {"enable_thinking": False,
                                                                   "preserve_thinking": True}}, {}),
    ("orca-sampling", "openorca", {"model": "qwen3.8-27b", "messages": [HI], "max_tokens": 32, "top_k": 20,
                                   "min_p": 0.0, "presence_penalty": 1.5, "repeat_penalty": 1.0,
                                   "repetition_penalty": 1.0}, {}),
    # risk 1: unknown / gateway fields
    ("orca-uncensored", "openorca", {"model": "qwen3.8-27b", "messages": [HI], "max_tokens": 32,
                                     "uncensored": True}, {}),
    ("unknown-metadata-user", "any", {"model": "qwen3.8-27b", "messages": [HI], "max_tokens": 32,
                                      "metadata": {"session_id": "s-1"}, "user": "tester",
                                      "prompt_cache_key": "pck-1"}, {}),
    ("extra-body-nested", "sdk", {"model": "qwen3.8-27b", "messages": [HI], "max_tokens": 32,
                                  "extra_body": {"chat_template_kwargs": {"enable_thinking": False},
                                                 "reasoning_effort": "high"}}, {}),
    ("top-level-enable-thinking", "pig", {"model": "qwen3.8-27b", "messages": [HI], "max_tokens": 32,
                                          "enable_thinking": False, "reasoning_effort": "medium"}, {}),
    ("pinned-b-stream", "bench", {"model": "qwen3.8-flash-next-b", "messages": [HI], "max_tokens": 32,
                                  "stream": True}, {}),
    ("catalog-live", "openorca", {"model": "inkling-2-mini", "messages": [HI], "max_tokens": 32}, {}),
    ("max-completion", "sdk", {"model": "qwen3.8-27b", "messages": [HI], "max_completion_tokens": 32,
                               "n": 1, "stop": ["\n\n"], "seed": 7}, {}),
]
