# Rust port status

Updated 2026-11-25. Plan and decisions: [`PORTING.md`](PORTING.md).

## Done (phase 1: foundation)

- Workspace (8 crates), pinned dependencies, CI job (`rust` in `.github/workflows/ci.yml`).
- Config: `OrcaConfig` model, load/save round-trip byte-compatible with C# (fixtures,
  BOM, first run, ported `ConfigManager` tests).
- OpenAI-compatible client: streaming SSE and non-streaming, tool-call assembly, sampling
  presets and thinking levels (`RequestParameters`), extra body and session field, served
  model tracking, timeouts; wire format matches the C# SDK byte for byte in the cross-check.
- Agent loop: tool registry, permission matching, tool pipeline (unknown tool hints,
  required-argument check, directory restriction, read-before-edit, timeouts, truncation
  with temp spill), empty-args downgrade, verification step and retry, run evidence
  (`RunEvidence` port), summary request and continuations.
- Tools: read_file, write_file, edit_file (fuzzy matching), list_directory, glob, grep,
  bash, get_process_output, stop_process.
- CLI: argument parsing as `CliArgumentParser`, `--print` JSON output, `--version`.
- Tests: 119 Rust tests (ported C# tests for config, client, request parameters, thinking,
  served models, permissions, file and shell tools, run evidence; e2e `--print`).
- Cross-check: 14 scenarios, all identical between the C# and Rust builds.

## Next (phase 2: agent loop parity)

Prompt system (`prompts/default.md`, project instructions, model notes), text tool calling
and nudges, retries and error recovery, doom-loop and retry-loop detection, context
management and compaction, sessions (`-c`, `-r`, autosave), interactive permission
prompter, hooks, checkpoints, plan mode / sandbox policies, test-failure gate, alias repair.

## Known gaps

- System prompt is a compact placeholder; tool list sent is the 9 ported tools (C# sends
  31+). Both masked in the cross-check (`--strict` shows them).
- No interactive mode (REPL/TUI) yet: without `--print`/`--prompt` the binary exits 2.
- No permission prompter: unapproved calls are denied (fail closed).
- No text-mode tool calling: after the empty-args downgrade the Rust build sends no tools
  but only acts on native tool calls.
- No retries, compaction, doom/retry-loop detection, delegated-check step, scratch-file
  cleanup exception in run evidence (no delete tool yet).
- grep: Rust regex syntax (no lookaround/backreferences); files in sorted order.
- Console output decoding is lossy UTF-8 rather than the OEM code page.
- list_directory ordering approximates .NET culture-aware sorting.
- The empty-args downgrade is not remembered across runs (C# `RememberNativeToolsSupport`).
- Live smoke (LiteLLM, qwen3.8-27b, read a file and answer) passed 2026-11-25; not part of CI.
