# Rust port status

Updated 2026-11-25. Plan and decisions: [`PORTING.md`](PORTING.md).

## Done

### Phase 1: foundation

- Workspace (8 crates), pinned dependencies, CI job (`rust` in `.github/workflows/ci.yml`).
- Config: `OrcaConfig` model, load/save round-trip byte-compatible with C# (fixtures,
  BOM, first run, ported `ConfigManager` tests).
- OpenAI-compatible client: streaming SSE and non-streaming, tool-call assembly, sampling
  presets and thinking levels (`RequestParameters`), extra body and session field, served
  model tracking, timeouts; wire format matches the C# SDK byte for byte in the cross-check.
- Tools: read_file, write_file, edit_file (fuzzy matching), list_directory, glob, grep,
  bash, get_process_output, stop_process.
- CLI: argument parsing as `CliArgumentParser`, `--print` JSON output, `--version`.

### Phase 2: agent loop parity (most of it)

- **Prompt system** (`config::prompt_manager`, `config::project_instructions`,
  `config::memory_manager`, `agent::system_prompt`, `agent::prompt_constants`): the shipped
  template and `prompts/default.md` with its version stamp and hash (byte-identical to C#),
  promptProfile / model slug / default.md resolution, legacy prompt hashes, project
  instructions (ORCA.md, AGENTS.md, CLAUDE.md), opt-in memory, native vs text tool protocol,
  model notes, peer / orchestrator / plan / sandbox / directory / thinking budget / Context7
  additions, `PLATFORM` as .NET prints it. The system prompt is compared in the cross-check
  (only the tool-derived lines are masked).
- **Turn loop** (`agent::turn`, `AgentLoopRunner` + `ToolCallDispatcher`): native and text
  tool calls (`agent::tool_call_parser`, all tag formats, JSON repair), truncated-call and
  code-block nudges, sticky native-tools downgrade with prompt rebuild, doom-loop guard,
  identical-failure warnings / STOP and the retry-loop redirect, failing-test gate, harness
  notes, dangling-call closing, task reminder, budget advisory, budget extension while the
  turn progresses (`agent::long_task`), stuck-probe steering, iteration-limit handoff with
  harness facts, same-model retry with backoff and `lmStudio.models` failover on idle
  timeout / empty replies, quality-based failover (`agent::response_quality`), verification
  and summary wind-down. `--print` reports `LastAssistantResponse` as C# does.
- **Context management** (`chat::*`, `agent::context`, `client::context_overflow`,
  `client::context_window_detector`): token estimates with server usage, pruning, squeezing,
  thinking-aware pass, planned/hard compaction with the LLM summary (Pi prompts), compact and
  retry once on a context overflow, repeat-call notes.
- **Policies and permissions** (`agent::policies`, `agent::permissions`): input size, plan
  mode, sandbox mode (`--sandbox`), orchestrator-Lead write ban; read-only tool lists in plan /
  sandbox mode; the permission prompter trait with session "yes to all" scopes.
- **Sessions, checkpoints, hooks** (`session::*`, `hooks::*`, `agent::tool_hooks`): session
  files compatible both ways with C#, `-c` / `-r` in `--print`, fork / tree / list, file
  checkpoints before modifications, pre/post tool hooks and the auto-formatter.

### Tests

- Rust: 501 tests (`cargo test --workspace`), clippy `-D warnings` and fmt clean.
- Cross-check: 28 scenarios, all identical between the C# and Rust builds (text tool calls,
  nudges, doom loops, iteration limit, sandbox, empty-args downgrade, sessions, hooks,
  checkpoints, compaction, plus the phase 1 set).

## Next

1. Phase 2 leftovers (see gaps below): mid-stream salvage, thinking-runaway and per-step
   reasoning caps (need a stream abort hook in the client), adaptive reasoning, the server
   error probe, `MaxTokensNegotiator`, the startup context-window probe, early read-only runs,
   plan mode entry points (`/plan`, planner phase), pair voting.
2. Phase 3: the remaining tools and plugins, Tools.Pi (and the Pi system prompt), sub-agents,
   orchestration, slots (SlotAllocator balanced placement, CopyModels / ServedModels, server
   identity with the gateway `/health` skip, preserve-thinking), engines, profiles.
3. Phase 4: REPL / TUI, slash commands, custom commands. Phase 5: WebUi host and hub.

## Known gaps

- Tool list: Rust advertises the 9 ported tools (C# 31+); masked in the cross-check
  (`--strict` shows it).
- No interactive mode (REPL/TUI) yet: without `--print`/`--prompt` the binary exits 2. The
  permission prompter exists but the CLI installs none, so calls needing approval are refused
  (C# `--print` behaves the same).
- Stream errors: a stream that dies mid-response loses its partial text (C# executes a complete
  tool call from it); the thinking-runaway cancel and the per-step reasoning cap are not wired
  (pure `chat::reasoning_cap` logic is ported).
- An empty reply counts as empty only without usage; C# counts any streamed content.
- Delegated-check request, background helper collection, lead-mode auto choice and read-only
  spawns in plan mode wait for the agent tools (phase 3).
- Sessions: `engine` / `engineSessionId` are dropped on save; no auto-save outside `--print`
  (there is no interactive loop yet).
- Numbers inside re-serialized tool arguments use serde's shortest form (`1.0` -> `1`), C#
  keeps the model's digits.
- grep: Rust regex syntax (no lookaround/backreferences); files in sorted order.
- Console output decoding is lossy UTF-8 rather than the OEM code page; hook stdin is UTF-8.
- list_directory ordering approximates .NET culture-aware sorting.
- Pi profile: the Pi system prompt is not ported (a Pi config gets the OpenOrca prompt).
- Live smoke (LiteLLM, qwen3.8-27b, read a file and answer) passed 2026-11-25 for phase 1.
