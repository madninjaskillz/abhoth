# Porting OpenOrca to Rust

The C# code (`src/`, `tests/`) stays the reference implementation and keeps working. The
Rust port lives in `rust/`, is a separate Cargo workspace, and is invisible to .NET: the
solution file lists its projects explicitly, and nothing under `rust/` is a .NET project.
Progress and gaps are tracked in [`STATUS.md`](STATUS.md).

## Building and testing

```sh
cd rust
cargo build                         # debug build; target/debug/openorca(.exe)
cargo test --workspace              # unit, ported C# tests, end-to-end --print tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The toolchain is pinned in `rust-toolchain.toml` (rustup picks it up). CI runs the same four
commands in the `rust` job of `.github/workflows/ci.yml`, on Linux and Windows.

Cross-check against the C# build (needs a Release build of the C# CLI):

```sh
dotnet build ../OpenOrca.sln -c Release
cargo build -p orca-cli -p orca-crosscheck
target/debug/orca-crosscheck --csharp ../src/OpenOrca.Cli/bin/Release/net10.0/openorca.exe \
    --rust target/debug/openorca.exe [--only <scenario>] [--out <dir>] [--strict]
```

## Crate map

| C# project / area                        | Rust crate            | State     |
|------------------------------------------|-----------------------|-----------|
| OpenOrca.Abstractions (`IOrcaTool`, `ToolResult`, capabilities, risk) | `orca-abstractions` | done |
| OpenOrca.Tools.Common (truncation, paths, text helpers) | `orca-tools-common` | done for current tools |
| OpenOrca.Core/Configuration (`OrcaConfig`, `ConfigManager`) | `orca-core::config` | done, round-trip compatible |
| OpenOrca.Core/Client (OpenAI client, extra body, served models, request parameters, thinking) | `orca-core::client` | done for LM Studio / OpenAI-compatible |
| OpenOrca.Cli/Repl agent loop (`AgentLoopRunner`, `ToolCallDispatcher`, `ToolCallExecutor`, `ToolExecutionPipeline`, `RunEvidence`) | `orca-core::agent` | core loop done; see gaps |
| OpenOrca.Core/Permissions | `orca-core::agent::permissions` | matcher done; no prompter yet |
| plugins/tools FileSystem (read, write, edit, list, glob, grep) | `orca-tools-fs` | done |
| plugins/tools Shell (bash, get_process_output, stop_process) | `orca-tools-shell` | done |
| plugins/tools (git, web, image, LSP, MCP proxy, archive, multi_edit, apply_patch, agents, ...) | `orca-tools-*` | phase 3 |
| plugins/commands (slash commands) | `orca-cli::commands` | phase 4 |
| OpenOrca.Tools.Pi (Pi profile toolset) | `orca-tools-pi` | phase 3 |
| OpenOrca.Core Session, Chat, Orchestration, Slots, Engines, Profiles, Hooks, Mcp, Live, Attachments | `orca-core::*` | phase 2-3 |
| OpenOrca.Cli (Program, REPL, rendering, TUI, custom commands, `--print`) | `orca-cli` | `--print` done; REPL/TUI phase 4 |
| OpenOrca.WebUi (host, SignalR hub, bench, git, files, PTY, projects) | `orca-webui` | phase 5 |
| tests/OpenOrca.TestKit (fake model server) | `orca-testkit` | done |
| (new) C#/Rust cross-check | `orca-crosscheck` | done |

## Library mapping

| .NET                                | Rust                                  | Notes / risk |
|-------------------------------------|---------------------------------------|--------------|
| System.Text.Json + JsonDefaults     | serde_json (`preserve_order`) + `orca_core::config::dotnet_json` | .NET escaping (JavaScriptEncoder: `<>&'+` and non-ASCII escaped in records; relaxed encoder for history echo) and indented CRLF output are reproduced by hand. Low risk; covered by byte-compat fixtures. |
| Microsoft.Extensions.AI + OpenAI SDK | reqwest + own SSE parser             | Wire format (key order, float form, message shapes) is checked by the cross-check. Medium risk: the SDK has undocumented quirks (see below). |
| Spectre.Console                     | crossterm + ratatui (phase 4)         | Rendering will not be byte-identical; TUI harness tests compare semantics. |
| ASP.NET Core + SignalR              | axum + tokio-tungstenite websockets   | See "SignalR". |
| Microsoft.Extensions.FileSystemGlobbing | globset (case-insensitive, literal separator) | Done. |
| .NET Regex                          | regex                                 | No lookaround/backreferences: grep with such patterns fails with a clear error; internal patterns are rewritten by hand (`run_evidence` splitter). |
| Process + ConPTY                    | tokio::process; portable-pty for PTY (phase 5) | Kill trees via `taskkill /T /F` / process groups. |
| xUnit                               | cargo test (+ tokio::test)            | Tests are ported with each part. |
| MCP client (ModelContextProtocol)   | rmcp or a small JSON-RPC client (phase 3) | Decide when phase 3 starts; prefer the official crate if mature. |
| Plugin assemblies loaded at runtime | compile-time registration             | See "Plugins". |

Dependencies are pinned exactly in the workspace `Cargo.toml` and `Cargo.lock` is
committed. New crates must be well-known; upgrades are deliberate.

## SignalR to websockets

The React client (`src/OpenOrca.WebUi/client`) stays. It talks to the hub through
`@microsoft/signalr`. Plan: the Rust host speaks plain JSON messages over one websocket
(`/hub/ws`), with the same method names and payloads as the hub's methods and client
callbacks (`{"type": "invoke", "id", "method", "args"}` / `{"type": "result", ...}` /
`{"type": "event", "method", "args"}`). The client gets a thin adapter with the
`HubConnection` surface it uses (`invoke`, `on`, `off`, `start`, `stop`, reconnect events)
so components don't change. Reconnect with resume (the current hub's reconnect handling)
is kept by the server keeping per-connection state for a grace period. Risk: medium
(streams and reconnect semantics); mitigated by porting the WebUi hub tests first.

## Plugins

C# loads `plugins/tools` and `plugins/commands` assemblies at runtime. Rust has no stable
ABI, so tools and commands are compiled in: each tool crate exposes `all_tools(...)` and
the CLI registers them (`orca_cli::build_registry`). Optional groups are Cargo features
(e.g. `image`, `lsp`) so a slimmer binary is possible. External extension stays available
through MCP servers and custom commands (markdown), which C# also supports. Third-party
DLL plugins are not supported by the port (decision D3).

## Phase plan

Sizes are rough (S < 1 day, M 1-3 days, L 3-7 days, XL > 1 week of agent time).

1. **Foundation** (done): workspace, config, OpenAI client, agent loop core, file and
   shell tools, `--print`, e2e tests, cross-check harness, CI job. M-L.
2. **Agent loop parity**: full prompt system (`prompts/default.md`, project instructions,
   model notes), text tool calling (parser, nudges), retries and error recovery, doom-loop
   and retry-loop detection, context management and compaction, sessions (save, resume,
   `-c`/`-r`), interactive permission prompter, hooks, checkpoints, plan mode and sandbox
   policies, test-failure gate, alias repair. L-XL.
3. **Tools**: the rest of plugins/tools (git, web, image, process, archive, multi_edit,
   apply_patch, LSP, MCP proxy, ask_user, task list, think), sub-agents and orchestration
   (spawn_agent(s), slots, engines, profiles), Tools.Pi. XL.
4. **CLI and TUI**: REPL, slash commands, rendering, TUI, custom commands, `--demo`. L-XL.
5. **WebUi host**: axum host, websocket hub + client adapter, bench, git, files, PTY,
   projects, profiles, settings, serving the built React client. XL.
6. **Parity sweep**: run the whole cross-check set and the ported test suites, bench runs
   side by side, then a decision on replacing the C# build (not part of this port). M.

## Parity strategy

1. **Ported tests**: each part brings its C# tests (same names in snake_case, same inputs
   and expected values). A test that cannot be ported says why in a comment.
2. **Byte-compat fixtures**: config files written by C# load and save back byte-identical.
3. **Cross-check harness** (`orca-crosscheck`): scripted sessions (`crosscheck/scenarios`)
   run through both binaries against the same fake server; the `--print` JSON, every
   request body and the resulting files must match after normalising temp paths and
   durations. Known gaps (system prompt text, advertised tool list) are masked unless
   `--strict`. New behaviour gets a scenario before or with its port.
4. **Live smoke** (optional): a real OpenAI-compatible server, only with a key already in
   the environment.

## C# behaviours to preserve (found while porting)

- **Wire format** (OpenAI .NET SDK): body key order is `temperature, top_p, messages,
  max_completion_tokens, presence_penalty, stream, stream_options, tools, tool_choice`, then
  the extra-body fields (top_k, additional properties, session field), then `model`. Whole
  floats are written without a fraction (`0`, not `0.0`). `max_completion_tokens`, not
  `max_tokens`.
- **History echo**: assistant tool-call arguments are re-serialized indented with platform
  line breaks (relaxed encoder: only non-BMP characters escaped as surrogate pairs);
  unparseable arguments are echoed as `null`. A tool-call-only assistant turn has content
  `""`. A streamed reply whose first content delta was empty is sent back as a two-part
  content array (`[{"type":"text","text":""}, {"type":"text","text":...}]`).
- **Tool records** (`--print` `tool_calls`): arguments indented with JavaScriptEncoder
  escaping (`&` as `&`), except the first of several calls in one response, which is
  recorded compact. Prelude failures (unknown tool, denied, missing arguments, read-before-
  edit) are not recorded; denied/blocked results have no `ERROR: ` prefix, executed-tool
  errors do.
- **Read-before-edit**: write/edit of an existing file not read this session (or changed on
  disk since) is refused with the exact C# message.
- **Empty-args downgrade**: a native tool call with empty arguments for a tool with required
  parameters drops that response and switches to text tool calling (no `tools` sent).
- **Wind-down**: after tools ran, a verification step is requested when files changed and
  no passing build/test run came after (`agent.verificationMode`), a retry when the
  verification reply used no tools, and a summary request only when the final message is
  not a substantive wrap-up (40+ chars, 8+ words); summary continuations on `length`.
- **Prompt constants** are raw string literals: they carry the source checkout's line
  breaks (CRLF on Windows), so the port sends them with platform line breaks.
- Text output uses `Environment.NewLine` wherever C# uses `AppendLine`; lengths and limits
  count UTF-16 units; `--print` omits null `tool_calls` / `files_modified`.
- `nativeToolCalling` defaults to false in config; cmd.exe fed through stdin prints its
  banner and exits 0 (same in both).

## Decisions log

- **D1** Separate Cargo workspace under `rust/`, edition 2024, toolchain pinned. Crates
  follow the C# project split so tests and parity checks map one to one.
- **D2** Exact version pins and a committed lockfile; only well-known crates.
- **D3** Tools and commands are compiled in (Cargo features for optional groups) instead of
  runtime plugin loading; MCP and markdown custom commands remain the extension points.
- **D4** SignalR is replaced by a JSON-over-websocket protocol with a client-side adapter
  that keeps the `HubConnection` surface; the React client stays.
- **D5** Wire and output quirks of the C# build are reproduced, not fixed, while the C#
  build is the reference: parity first, improvements later behind explicit decisions.
- **D6** Grep uses the `regex` crate: patterns needing lookaround/backreferences fail with
  an error instead of pulling in a backtracking engine (fancy-regex can be added later).
- **D7** The cross-check masks the system prompt text and the tools list until phase 2
  ports the prompt system and phase 3 the remaining tools.
- **D8** The run-evidence command splitter is hand-written (the C# regex uses lookaround).
