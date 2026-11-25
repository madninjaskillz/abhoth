//! Port of `SystemPromptBuilder.cs` (OpenOrca tool profile): the system prompt from the prompt
//! template, tool signatures, project instructions, opted-in memory and the mode additions
//! (peer / orchestrator, plan mode, thinking budget, sandbox, directory restriction, Context7,
//! next-prompt instruction), and the Pi tool profile's prompt (`BuildPiPromptAsync`).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use super::prompt_constants::{
    CONTEXT7_SYSTEM_PROMPT_ADDITION, NEXT_PROMPT_INSTRUCTION, PI_PLAN_MODE_SYSTEM_PROMPT_ADDITION,
    PLAN_MODE_SYSTEM_PROMPT_ADDITION, platform_lines,
};
use super::prompts::format_signature;
use super::registry::ToolRegistry;
use crate::config::OrcaConfig;
use crate::config::memory_manager::MemoryManager;
use crate::config::project_instructions;
use crate::config::prompt_manager::{
    PromptManager, detect_model_family, get_model_notes, get_tool_protocol_section,
};

/// A tool as the system prompt lists it (C#: an `AIFunction`'s name, description and schema).
#[derive(Debug, Clone)]
pub struct PromptTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

impl PromptTool {
    pub fn new(name: impl Into<String>, description: impl Into<String>, schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            schema,
        }
    }

    /// Every advertised tool of `registry`, in advertised order.
    pub fn from_registry(registry: &ToolRegistry) -> Vec<PromptTool> {
        registry
            .advertised()
            .iter()
            .map(|t| PromptTool::new(t.name(), t.description(), t.parameter_schema()))
            .collect()
    }
}

/// `SystemPromptBuilder`.
pub struct SystemPromptBuilder {
    prompt_manager: PromptManager,
    memory_manager: Option<MemoryManager>,
    cwd: PathBuf,
    platform: String,
    /// `ReplState.NativeToolsSupport`: per model (case-insensitive), whether native tool calls
    /// proved to work.
    native_tools_support: Mutex<HashMap<String, bool>>,
    /// `_promptModes`: the tool-calling mode each prompt this builder produced was built for,
    /// keyed by a hash of the prompt text (C# keys by string reference).
    prompt_modes: Mutex<HashMap<u64, bool>>,
}

impl SystemPromptBuilder {
    pub fn new(
        prompt_manager: PromptManager,
        memory_manager: Option<MemoryManager>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            prompt_manager,
            memory_manager,
            cwd: cwd.into(),
            platform: os_version_string(),
            native_tools_support: Mutex::new(HashMap::new()),
            prompt_modes: Mutex::new(HashMap::new()),
        }
    }

    /// Prompts and memory under the default config dir, project from `cwd`.
    pub fn with_defaults(config: &OrcaConfig, cwd: &Path) -> Self {
        Self::new(
            PromptManager::new(),
            Some(MemoryManager::new(config).with_cwd(cwd)),
            cwd,
        )
    }

    pub fn prompt_manager(&self) -> &PromptManager {
        &self.prompt_manager
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `RememberNativeToolsSupport`: record whether `model` handles native tool calls.
    pub fn remember_native_tools_support(&self, model: &str, supported: bool) {
        self.native_tools_support
            .lock()
            .unwrap()
            .insert(model.to_lowercase(), supported);
    }

    /// What [`Self::remember_native_tools_support`] recorded for `model`.
    pub fn native_tools_support(&self, model: &str) -> Option<bool> {
        self.native_tools_support
            .lock()
            .unwrap()
            .get(&model.to_lowercase())
            .copied()
    }

    /// `ResolveNativeToolCalling`: native iff the config enables it, there are tools to send,
    /// and the primary model has not proved it cannot do native calls.
    pub fn resolve_native_tool_calling(
        &self,
        config: &OrcaConfig,
        tools: Option<&[PromptTool]>,
    ) -> bool {
        if !config.lm_studio.native_tool_calling || tools.is_none_or(|t| t.is_empty()) {
            return false;
        }
        if let Some(model) = config.lm_studio.primary_model()
            && let Some(supported) = self.native_tools_support(model)
        {
            return supported;
        }
        true
    }

    /// `GetPromptToolMode`: the tool-calling mode `prompt` was built for, or `None` when this
    /// builder did not produce it (custom or restored prompts are left alone).
    pub fn get_prompt_tool_mode(&self, prompt: Option<&str>) -> Option<bool> {
        let prompt = prompt?;
        self.prompt_modes
            .lock()
            .unwrap()
            .get(&text_key(prompt))
            .copied()
    }

    /// `GetSystemPromptAsync(tools, planMode)`: with the resolved tool-calling mode.
    pub fn get_system_prompt(
        &self,
        config: &OrcaConfig,
        tools: Option<&[PromptTool]>,
        plan_mode: bool,
    ) -> String {
        let native = self.resolve_native_tool_calling(config, tools);
        self.get_system_prompt_for_mode(config, tools, plan_mode, native)
    }

    /// `GetSystemPromptAsync(tools, planMode, nativeToolCalling)`.
    pub fn get_system_prompt_for_mode(
        &self,
        config: &OrcaConfig,
        tools: Option<&[PromptTool]>,
        plan_mode: bool,
        native_tool_calling: bool,
    ) -> String {
        if orca_tools_pi::is_pi_profile(&config.tool_profile) {
            let pi_prompt = self.build_pi_prompt(config, tools, plan_mode);
            self.prompt_modes
                .lock()
                .unwrap()
                .insert(text_key(&pi_prompt), native_tool_calling);
            return pi_prompt;
        }

        let has_tool =
            |pred: &dyn Fn(&str) -> bool| tools.is_some_and(|t| t.iter().any(|x| pred(&x.name)));

        // Compact typed signatures: in text mode this is the ONLY place the model learns
        // parameter names and types.
        let tool_list = match tools {
            Some(tools) => tools
                .iter()
                .map(|t| {
                    format!(
                        "  - {}",
                        format_signature(&t.name, Some(&t.description), &t.schema)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            None => "  (no tools available)".to_string(),
        };

        let model_id = config.lm_studio.primary_model().unwrap_or("");
        let family = detect_model_family(model_id);

        // Project instructions: ORCA.md, AGENTS.md, CLAUDE.md (each labelled by path)
        let project_instructions =
            project_instructions::format_for_prompt(&project_instructions::load_all(&self.cwd));

        let cwd = self.cwd.display().to_string();
        let max_iterations = config.agent.main_loop_max_iterations.max(1).to_string();
        let variables: [(&str, String); 7] = [
            ("TOOL_LIST", tool_list),
            ("CWD", cwd),
            ("PLATFORM", self.platform.clone()),
            (
                "TOOL_PROTOCOL",
                get_tool_protocol_section(native_tool_calling, family).to_string(),
            ),
            ("MODEL_NOTES", get_model_notes(family).to_string()),
            ("MAX_ITERATIONS", max_iterations),
            (
                "PROJECT_INSTRUCTIONS",
                project_instructions.clone().unwrap_or_default(),
            ),
        ];

        let mut result = self.prompt_manager.load_system_prompt(
            config.lm_studio.prompt_profile.as_deref(),
            model_id,
            &variables,
        );

        // Append project instructions unless the template placed them itself
        if let Some(pi) = project_instructions.as_deref()
            && !pi.trim().is_empty()
            && !result.contains(pi)
        {
            result.push_str("\n\n");
            result.push_str(pi);
        }

        // Auto memory: opt-in (model-written notes are unreviewed)
        if let Some(memory) = &self.memory_manager
            && config.memory.include_in_prompt
        {
            let content = memory.load_all_memory();
            if !content.trim().is_empty() {
                result.push_str("\n\nPROJECT MEMORY (auto-learned):\n");
                result.push_str(&content);
            }
        }

        let can_spawn = has_tool(&|n| n == "spawn_agent" || n == "spawn_agents");
        let is_orchestrator_lead = !config.agent.peer_mode
            && config
                .agent
                .lead_mode
                .trim()
                .eq_ignore_ascii_case("orchestrator");
        if config.agent.peer_mode {
            result = apply_peer_mode(&result, can_spawn);
        } else if is_orchestrator_lead && can_spawn {
            result = apply_orchestrator_mode(&result, has_tool(&|n| n == "bash"));
        } else if has_tool(&|n| n == LEAD_MODE_TOOL_NAME) {
            result.push_str("\n\nLEAD MODE (auto):\n");
            result.push_str(platform_lines(LEAD_MODE_AUTO_INSTRUCTIONS).trim());
        }

        if plan_mode {
            result.push_str("\n\n");
            result.push_str(&platform_lines(PLAN_MODE_SYSTEM_PROMPT_ADDITION));
        }

        if config.thinking.budget_tokens > 0 {
            result.push_str(&format!(
                "\n\nThinking budget: You have a budget of {} tokens for <think> reasoning. Use them wisely \u{2014} keep thinking brief for simple tasks, thorough for complex ones.",
                config.thinking.budget_tokens
            ));
        }

        if config.sandbox_mode {
            result.push_str("\n\nSANDBOX MODE: You only have read-only tools available. You cannot modify files, run commands, or make changes.");
        }

        if let Some(dir) = &config.allowed_directory {
            result.push_str(&format!(
                "\n\nDIRECTORY RESTRICTION: File operations are restricted to: {dir}"
            ));
        }

        // Context7 MCP tools: inject documentation instructions
        if has_tool(&|n| n.to_lowercase().ends_with("resolve-library-id")) {
            result.push_str("\n\n");
            result.push_str(&platform_lines(CONTEXT7_SYSTEM_PROMPT_ADDITION));
        }

        result.push_str("\n\n");
        result.push_str(NEXT_PROMPT_INSTRUCTION);

        self.prompt_modes
            .lock()
            .unwrap()
            .insert(text_key(&result), native_tool_calling);
        result
    }
}

impl SystemPromptBuilder {
    /// `BuildPiPromptAsync`: Pi's own system prompt (`PiSystemPrompt.Build`) with the project
    /// instructions (and opted-in memory) as `<project_context>` and OpenOrca's addenda.
    fn build_pi_prompt(
        &self,
        config: &OrcaConfig,
        tools: Option<&[PromptTool]>,
        plan_mode: bool,
    ) -> String {
        use orca_tools_pi::{PiContextFile, lead_mode, subagent, system_prompt};

        let cwd = self.cwd.display().to_string();
        let names: Vec<String> = tools
            .map(|t| t.iter().map(|x| x.name.clone()).collect())
            .unwrap_or_default();
        let has = |n: &str| names.iter().any(|x| x == n);

        let mut context_files: Vec<PiContextFile> = project_instructions::load_all(&self.cwd)
            .into_iter()
            .map(|f| PiContextFile {
                path: f.full_path.display().to_string(),
                content: f.content,
            })
            .collect();
        // Opted-in memory stays Pi-shaped: one more <project_instructions> entry per memory dir.
        if let Some(memory) = &self.memory_manager
            && config.memory.include_in_prompt
        {
            for (dir, content) in memory.load_memory_sources() {
                context_files.push(PiContextFile {
                    path: dir.display().to_string(),
                    content,
                });
            }
        }

        let agent = &config.agent;
        let mut addenda: Vec<String> = Vec::new();
        let peer = agent.peer_mode && has(subagent::TOOL_NAME);
        if peer {
            addenda.push(subagent::peer_instructions(agent.peer_background));
        } else if has(subagent::TOOL_NAME) {
            addenda.push(if super::policies::is_orchestrator_lead(config) {
                subagent::orchestrator_instructions(has("bash"))
            } else if has(lead_mode::TOOL_NAME) {
                platform_lines(lead_mode::AUTO_INSTRUCTIONS)
                    .trim()
                    .to_string()
            } else {
                platform_lines(&subagent::LEAD_INSTRUCTIONS)
            });
        }
        let is_swarm = agent
            .provider_schedule
            .as_ref()
            .is_some_and(|s| !s.is_empty())
            || agent.max_agents > 0;
        if !peer && has(subagent::TOOL_NAME) && is_swarm {
            addenda.push(subagent::swarm_instructions(agent.max_agents));
        }
        if let Some(append) = config.pi.append_system_prompt.as_deref()
            && !append.trim().is_empty()
        {
            addenda.push(append.trim().to_string());
        }
        if plan_mode {
            addenda.push(platform_lines(PI_PLAN_MODE_SYSTEM_PROMPT_ADDITION));
        }
        if config.sandbox_mode {
            addenda.push("SANDBOX MODE: You only have read-only tools available. You cannot modify files, run commands, or make changes.".into());
        }
        if let Some(dir) = &config.allowed_directory {
            addenda.push(format!(
                "DIRECTORY RESTRICTION: File operations are restricted to: {dir}"
            ));
        }
        // Adaptive reasoning runs the thinking steps at medium effort; this is the low effort's sentence.
        if agent.reasoning.is_adaptive(true) && agent.reasoning.adaptive_brief_rule {
            addenda.push(ADAPTIVE_BRIEF_RULE.into());
        }

        let mut snippets = orca_tools_pi::prompt_snippets();
        let mut guidelines = orca_tools_pi::prompt_guidelines();
        if peer {
            // The subagent tool's peer wording in <tools> and <rules>.
            use orca_tools_pi::PiToolInfo;
            let peer_tool = subagent::PiSubagentTool::new(subagent::PiSubagentSettings {
                peer_mode: true,
                ..Default::default()
            });
            snippets.insert(subagent::TOOL_NAME.into(), peer_tool.prompt_snippet());
            guidelines.insert(subagent::TOOL_NAME.into(), peer_tool.prompt_guidelines());
        }

        let addendum = (!addenda.is_empty()).then(|| {
            addenda.join(
                "

",
            )
        });
        system_prompt::build(
            &names,
            &snippets,
            &guidelines,
            &context_files,
            &cwd,
            addendum.as_deref(),
            config.pi.batch_calls_rule,
        )
    }
}

/// `AdaptiveReasoning.BriefRule`.
pub const ADAPTIVE_BRIEF_RULE: &str = "Keep your thinking brief and focused, moving directly to the conclusion without unnecessary elaboration.";

fn text_key(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// `ApplyOrchestratorMode` (agent.leadMode = "orchestrator"): drop the hands-on delegation
/// section and append the orchestrator workflow.
pub fn apply_orchestrator_mode(prompt: &str, bash: bool) -> String {
    let mut prompt = prompt.to_string();
    if let Some(start) = prompt.find("AGENT DELEGATION")
        && let Some(rel) = prompt[start..].find("PROGRESS:")
        && rel > 0
    {
        prompt = format!("{}{}", &prompt[..start], &prompt[start + rel..]);
    }
    let lines: Vec<String> = prompt
        .split('\n')
        .filter(|l| !l.contains("Do trivial things yourself"))
        .map(|l| {
            if l.contains("Execute with tools (write_file") {
                let indent = &l[..l.len() - l.trim_start().len()];
                format!("{indent}4. Have agents make every change (see ORCHESTRATOR MODE)")
            } else {
                l.replace("(see AGENT DELEGATION)", "(see ORCHESTRATOR MODE)")
            }
        })
        .collect();
    format!("{}\n\n{}", lines.join("\n"), orchestrator_section(bash))
}

/// `ApplyPeerMode` (agent.peerMode): the delegation section becomes the peer section (or goes
/// when no agent tool is advertised), and the footer bar loses its "Lead" name.
pub fn apply_peer_mode(prompt: &str, can_spawn: bool) -> String {
    let mut prompt = prompt.to_string();
    let range = prompt.find("AGENT DELEGATION").and_then(|start| {
        prompt[start..]
            .find("PROGRESS:")
            .filter(|rel| *rel > 0)
            .map(|rel| (start, start + rel))
    });
    if let Some((start, end)) = range {
        let peer = if can_spawn {
            format!("{PEER_SECTION}\n\n")
        } else {
            String::new()
        };
        prompt = format!("{}{peer}{}", &prompt[..start], &prompt[end..]);
    } else if can_spawn {
        prompt.push_str("\n\n");
        prompt.push_str(PEER_SECTION);
    }
    prompt
        .replace(
            "it moves your own \"Lead\" bar in the footer",
            "it moves your own bar in the footer",
        )
        .replace("(see AGENT DELEGATION)", "(see HELPER AGENTS)")
}

/// `PiLeadModeTool.ToolName`.
pub const LEAD_MODE_TOOL_NAME: &str = "lead_mode";

/// `PiLeadModeTool.AutoInstructions`: the Lead's addendum in auto mode (a C# raw string
/// literal: sent with platform line breaks).
pub const LEAD_MODE_AUTO_INSTRUCTIONS: &str = r#"You lead a team of subagents. Every new user request gets ONE lead_mode call that sizes it, before you write, edit or delegate:
- delegate: big multi-part work — 3 or more files and roughly 300 or more new lines, or an investigation plus an implementation. Its result gives you the orchestrator workflow, and you cannot write or edit files for the rest of that request.
- hands-on: one file, a small fix, or tightly coupled work under roughly 300 new lines even across several files (every part needs the same new core). You do it yourself.
- answer: a question or an explanation; nothing changes.
Wall-clock time matters most, so:
- Read in parallel: put every read you need into ONE turn (several read/bash calls at once). Start with the file list and the spec, size the request from them, and call lead_mode in your next turn together with any reads you still need.
- Think briefly. To delegate, plan only the split: the pieces, the files each owns, the spec section each follows. The workers read the code and design it; do not work out code, formats or test cases yourself, and read only what you need to split the work.
- Hands-on: write whole files (several writes in one turn), then run the tests and the spec's sample in one bash call before you change anything. When they pass, stop and answer in at most 3 short lines, no lists."#;

/// `SystemPromptBuilder.PeerSection`:the OpenOrca-profile peer section.
pub const PEER_SECTION: &str = concat!(
    "HELPER AGENTS \u{2014} YOU DO THE WORK; HAND OFF ONLY INDEPENDENT PIECES:\n",
    "Helper agents (the same model as you) can take pieces of the work at the same time, but each starts cold and has to be briefed and checked.\n",
    "- Work alone on small tasks (a fix, one or two files), on sequential or tightly coupled work, and on reading you need for your own decisions.\n",
    "- Hand off only when the task splits into independent pieces that can run at the same time: separate files, modules or tests, with no file edited by two pieces. ",
    "Then send them in ONE spawn_agents call (agent_type=\"general\"), each with a `label` of at most 4 words and a self-contained task: the goal, the files it owns, ",
    "the interfaces it uses or provides, and the command that must pass. Helpers cannot see your messages but can read every file.\n",
    "- When they return, integrate their work and run the full test suite. If a helper failed or stopped early, finish that piece yourself.\n",
    "Each helper shows in the footer as a lettered progress bar (Agent A, Agent B, \u{2026}); when you report what one did, start with its tag, e.g. \"Agent A: added the parser\"."
);

/// `SystemPromptBuilder.OrchestratorSection`.
pub fn orchestrator_section(bash: bool) -> String {
    let mut s = String::from(
        "ORCHESTRATOR MODE \u{2014} YOU DELEGATE EVERY CHANGE (this overrides any rule above about writing or editing files yourself):\n\
         You cannot create, edit or delete files: you have no write_file, edit_file or other file-changing tool",
    );
    if bash {
        s.push_str(", and you must never change files through bash (no redirects, sed -i, rm, mv, git checkout or scripts that write)");
    }
    s.push_str(". Every change to the code, even a one-line fix, is made by a sub-agent.\n");
    s.push_str(
        "1. Read the spec and the code you need, then split the work into independent pieces.\n",
    );
    s.push_str("2. Send them in ONE spawn_agents call: agent_type=\"general\" for code, \"explore\" for investigation, each with a `label` of at most 4 words (list the biggest piece first: it starts first, on a model copy of its own) and a short, self-contained brief (at most about 120 words): the goal, the files it owns, where its rules are (point to the spec section; do not copy it), only the interfaces between pieces that the spec leaves open, and the command that must pass. Never work out expected values or outputs yourself for a brief; the spec and the tests are the reference. Agents cannot see your messages, but they can read every file. Tests and docs go with the code: each agent writes and runs the tests for its own piece, in its own test file (at most 12 focused cases), and updates the docs its piece changes; never a separate tests-only or docs-only agent. Ask only for what the spec asks: no extra options, flags or files, and no open invitations such as \"or an override if any\".\n");
    s.push_str(if bash {
        "3. Final check: ONE bash call that runs the full test suite and the spec's sample command(s) together. At most 1 more short probe, only for a spec rule the tests do not cover. When these pass you are done: do not spawn a reviewer unless the user asks for a review, and answer at once in at most 5 short lines (what was built, by which agent, the test result).\n"
    } else {
        "3. Review the results: have an agent run the full test suite and the spec's sample command(s) and report the exact failures within 10 steps (you have no shell). When they pass you are done: do not spawn a reviewer unless the user asks for a review, and answer in about 5 lines.\n"
    });
    s.push_str("4. Send each fix back to the agent that wrote that code: spawn_agent with `to` set to its letter (e.g. \"A\") and the fix as the task. It keeps its context, so say only what is new. Use a new agent only for new work. Repeat until the checks pass.\n");
    s.push_str("If the user only asks a question or wants an explanation and nothing needs to change, answer it directly without agents.");
    s
}

/// `Environment.OSVersion.ToString()`: "Microsoft Windows NT 10.0.<build>.0" on Windows,
/// "Unix <major>.<minor>.<build>.<revision>" from the kernel release elsewhere.
pub fn os_version_string() -> String {
    #[cfg(windows)]
    {
        let v = windows_version::OsVersion::current();
        // .NET's revision is the service pack ((major << 16) | minor), 0 on modern Windows.
        format!(
            "Microsoft Windows NT {}.{}.{}.{}",
            v.major,
            v.minor,
            v.build,
            v.pack << 16
        )
    }
    #[cfg(not(windows))]
    {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .or_else(|| {
                std::process::Command::new("uname")
                    .arg("-r")
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            })
            .unwrap_or_default();
        unix_os_version(release.trim())
    }
}

/// .NET's `GetOperatingSystem(release)` on Unix: the first four digit runs of the kernel
/// release ("6.8.0-45-generic" → "Unix 6.8.0.45"), missing ones 0.
pub fn unix_os_version(release: &str) -> String {
    let mut parts = [0u64; 4];
    let mut runs = release
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    for p in parts.iter_mut() {
        match runs.next() {
            Some(r) => *p = r.parse().unwrap_or(0),
            None => break,
        }
    }
    format!("Unix {}.{}.{}.{}", parts[0], parts[1], parts[2], parts[3])
}
