//! Port of `PromptManager.cs` and `LegacyPromptHashes.cs`: the system prompt template files in
//! `<config dir>/prompts` (`default.md` with its version stamp, per-model and profile files),
//! their upgrade rules, the shipped template, the tool-calling protocol sections and the
//! model-family notes.
//!
//! Hashing and delivery work on the header-stripped, `\n`-normalized body, so the shipped body
//! hash is the same on every platform and equal to the C# build's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use regex::Regex;
use sha2::{Digest, Sha256};

static NON_ALPHANUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9\-]").unwrap());
static MULTI_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-+").unwrap());
static STAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<!--\s*openorca-prompt\s+v(?<ver>\d+)\s+sha256:(?<hash>[0-9a-f]{64})\s*-->")
        .unwrap()
});
static EXCESS_BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// `PromptManager.CurrentPromptVersion`: written into the stamp of generated prompt files.
/// Informational: staleness is decided by the body hash.
pub const CURRENT_PROMPT_VERSION: i32 = 3;

/// `PromptManager.DefaultPromptFileName`.
pub const DEFAULT_PROMPT_FILE_NAME: &str = "default.md";

/// Classification of an on-disk prompt template (`PromptFileStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptFileStatus {
    /// The file did not exist (and was created).
    Missing,
    /// Unedited generated file matching the shipped prompt.
    Current,
    /// Unedited generated file (valid stamp) from an older OpenOrca: safe to upgrade.
    StaleGenerated,
    /// Unedited pre-stamp generated file (matches a known historical output): safe to upgrade.
    LegacyGenerated,
    /// The user has edited the file (or wrote it from scratch): never overwritten.
    UserEdited,
}

/// Manages the prompt template files.
pub struct PromptManager {
    prompts_dir: PathBuf,
    /// Test seam: warnings are also recorded here (C# tests use a recording logger).
    warnings: Option<Arc<Mutex<Vec<String>>>>,
}

impl PromptManager {
    /// Prompts under `<config dir>/prompts` (`OPENORCA_HOME` or `~/.openorca`).
    pub fn new() -> Self {
        Self::with_dir(super::default_config_dir().join("prompts"))
    }

    /// Prompts under `<config_dir>/prompts`.
    pub fn for_config_dir(config_dir: &Path) -> Self {
        Self::with_dir(config_dir.join("prompts"))
    }

    /// Test seam: an explicit prompts directory.
    pub fn with_dir(prompts_dir: impl Into<PathBuf>) -> Self {
        Self {
            prompts_dir: prompts_dir.into(),
            warnings: None,
        }
    }

    /// Record every warning in `sink` too (besides logging it).
    pub fn record_warnings(mut self, sink: Arc<Mutex<Vec<String>>>) -> Self {
        self.warnings = Some(sink);
        self
    }

    pub fn prompts_directory(&self) -> &Path {
        &self.prompts_dir
    }

    fn warn(&self, message: String) {
        tracing::warn!("{message}");
        if let Some(sink) = &self.warnings {
            sink.lock().unwrap().push(message);
        }
    }

    /// `EnsureDefaultPromptAsync`: make sure `prompts/default.md` exists and is current. An
    /// unedited generated file (stamp with matching body hash, or a byte-for-byte match with a
    /// pre-stamp generated file) is regenerated whenever the shipped prompt changes; a file the
    /// user edited is never touched (a one-line warning is logged instead).
    pub fn ensure_default_prompt(&self) -> std::io::Result<PromptFileStatus> {
        std::fs::create_dir_all(&self.prompts_dir)?;
        let default_path = self.prompts_dir.join(DEFAULT_PROMPT_FILE_NAME);
        if !default_path.exists() {
            std::fs::write(&default_path, generate_default_template())?;
            tracing::info!(target: "orca_core::PromptManager",
                "Created default prompt template: {}",
                default_path.display()
            );
            return Ok(PromptFileStatus::Missing);
        }

        let status = classify_template(&read_text(&default_path)?);
        match status {
            PromptFileStatus::StaleGenerated | PromptFileStatus::LegacyGenerated => {
                std::fs::write(&default_path, generate_default_template())?;
                tracing::info!(
                    "Upgraded unedited generated prompt {} ({status:?}) to shipped prompt v{CURRENT_PROMPT_VERSION}",
                    default_path.display()
                );
            }
            PromptFileStatus::UserEdited => self.warn(format!(
                "Prompt {} has been customized, so it was not upgraded to shipped prompt v{CURRENT_PROMPT_VERSION}; delete it to regenerate",
                default_path.display()
            )),
            _ => {}
        }
        Ok(status)
    }

    /// `LoadSystemPromptAsync`: the template for the current model with the variables
    /// substituted. Resolution: `profile_name` → `prompts/<model slug>.md` → `prompts/default.md`
    /// → the shipped template. Unedited generated files from an older OpenOrca are skipped.
    /// Leading HTML comment lines (the header) are stripped; line breaks become `\n`, runs of
    /// blank lines collapse to one and the result is trimmed.
    pub fn load_system_prompt(
        &self,
        profile_name: Option<&str>,
        model_id: &str,
        variables: &[(&str, String)],
    ) -> String {
        let mut template: Option<String> = None;

        // 1. Explicit profile override
        if let Some(profile) = profile_name.filter(|p| !p.trim().is_empty()) {
            let path = self.prompts_dir.join(format!("{profile}.md"));
            if path.is_file() {
                template = self.read_template(&path);
            } else {
                self.warn(format!(
                    "Configured promptProfile '{profile}' not found at {}",
                    path.display()
                ));
            }
        }

        // 2. Per-model file (user-created or legacy auto-generated)
        if template.is_none() && !model_id.trim().is_empty() {
            let path = self.prompts_dir.join(format!("{}.md", slugify(model_id)));
            if path.is_file() {
                template = self.read_template(&path);
            }
        }

        // 3. default.md
        if template.is_none() {
            let path = self.prompts_dir.join(DEFAULT_PROMPT_FILE_NAME);
            if path.is_file() {
                template = self.read_template(&path);
            }
        }

        // 4. Shipped template
        let mut template = template.unwrap_or_else(|| shipped_template_body().to_string());

        for (key, value) in variables {
            template = template.replace(&format!("{{{{{key}}}}}"), value);
        }

        EXCESS_BLANK_LINES
            .replace_all(&template.replace("\r\n", "\n"), "\n\n")
            .trim()
            .to_string()
    }

    /// A prompt file's body, or `None` when it is an unedited generated prompt from an older
    /// OpenOrca (resolution falls through) or cannot be read.
    fn read_template(&self, path: &Path) -> Option<String> {
        let content = match read_text(path) {
            Ok(c) => c,
            Err(e) => {
                self.warn(format!("Could not read prompt {}: {e}", path.display()));
                return None;
            }
        };
        let status = classify_template(&content);
        if matches!(
            status,
            PromptFileStatus::StaleGenerated | PromptFileStatus::LegacyGenerated
        ) {
            tracing::debug!(
                "Ignoring outdated unedited generated prompt {} ({status:?})",
                path.display()
            );
            return None;
        }
        tracing::debug!("Loaded prompt from {} ({status:?})", path.display());
        Some(strip_header(&content))
    }
}

impl Default for PromptManager {
    fn default() -> Self {
        Self::new()
    }
}

/// `File.ReadAllText`: UTF-8 with the BOM removed.
fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// `PromptManager.Slugify`: a model id as a safe file name, e.g. `qwen/qwen3-32b` →
/// `qwen-qwen3-32b`.
pub fn slugify(model_id: &str) -> String {
    let slug = model_id.trim().to_lowercase();
    let slug = NON_ALPHANUM.replace_all(&slug, "-");
    let slug = MULTI_DASH.replace_all(&slug, "-");
    slug.trim_matches('-').to_string()
}

/// `PromptManager.DetectModelFamily`: the model family by substring.
pub fn detect_model_family(model_id: &str) -> &'static str {
    let lower = model_id.to_lowercase();
    if lower.contains("qwen") {
        "qwen"
    } else if lower.contains("llama") {
        "llama"
    } else if lower.contains("mistral") {
        "mistral"
    } else if lower.contains("deepseek") || lower.contains("r1") {
        "deepseek"
    } else if lower.contains("gpt") {
        "gpt"
    } else {
        "default"
    }
}

/// `PromptManager.ClassifyTemplate`: current/stale generated, legacy (pre-stamp) generated, or
/// user-edited.
pub fn classify_template(content: &str) -> PromptFileStatus {
    let normalized = content.replace("\r\n", "\n");
    let normalized = normalized.trim_start_matches('\u{FEFF}');
    let body_hash = hash_template_body(normalized);
    if let Some(stamp) = STAMP.captures(normalized.trim_start()) {
        if stamp["hash"] != body_hash {
            return PromptFileStatus::UserEdited;
        }
        return if body_hash == shipped_body_hash() {
            PromptFileStatus::Current
        } else {
            PromptFileStatus::StaleGenerated
        };
    }
    if LEGACY_PROMPT_HASHES.contains(&body_hash.as_str()) {
        PromptFileStatus::LegacyGenerated
    } else {
        PromptFileStatus::UserEdited
    }
}

/// `PromptManager.StripHeader`: `\n` line breaks, without the leading header (HTML comment
/// lines and blank lines) and trailing whitespace. Hashing and delivery both use this form.
pub fn strip_header(content: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized
        .trim_start_matches('\u{FEFF}')
        .split('\n')
        .collect();
    let mut i = 0;
    while i < lines.len()
        && (lines[i].trim().is_empty()
            || (lines[i].trim_start().starts_with("<!--") && lines[i].trim_end().ends_with("-->")))
    {
        i += 1;
    }
    lines[i..].join("\n").trim_end().to_string()
}

/// `PromptManager.HashTemplateBody`: SHA-256 (lowercase hex) of [`strip_header`]'s output.
pub fn hash_template_body(content: &str) -> String {
    let digest = Sha256::digest(strip_header(content).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

static SHIPPED_TEMPLATE_BODY: LazyLock<String> = LazyLock::new(|| {
    strip_header(&format!(
        "{BASE_PROMPT}\n\n{{{{TOOL_PROTOCOL}}}}\n\n{{{{MODEL_NOTES}}}}\n\n{PROMPT_FOOTER}"
    ))
});
static SHIPPED_BODY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_template_body(&SHIPPED_TEMPLATE_BODY));

/// `PromptManager.ShippedTemplateBody`: the shipped template body (no header), with runtime
/// placeholders.
pub fn shipped_template_body() -> &'static str {
    &SHIPPED_TEMPLATE_BODY
}

fn shipped_body_hash() -> &'static str {
    &SHIPPED_BODY_HASH
}

/// `PromptManager.GenerateDefaultTemplate`: the contents written to `prompts/default.md`: a
/// version stamp carrying the body hash, a short header and the shipped body (`\n` breaks).
pub fn generate_default_template() -> String {
    format!(
        "<!-- openorca-prompt v{CURRENT_PROMPT_VERSION} sha256:{} -->\n\
         <!-- Generated by OpenOrca. While unedited it is upgraded automatically when OpenOrca ships a better prompt. -->\n\
         <!-- Once you edit it, OpenOrca leaves it alone (delete it to get the current default back). -->\n\
         <!-- Placeholders: TOOL_PROTOCOL (native or text tool-calling instructions, chosen at runtime), MODEL_NOTES, MAX_ITERATIONS, CWD, PLATFORM, TOOL_LIST, PROJECT_INSTRUCTIONS \u{2014} each wrapped in double braces. -->\n\
         \n{}\n",
        shipped_body_hash(),
        shipped_template_body()
    )
}

/// `PromptManager.GetToolProtocolSection`: native mode never teaches the text `<tool_call>`
/// format; text mode teaches only that.
pub fn get_tool_protocol_section(native_tool_calling: bool, model_family: &str) -> &'static str {
    if native_tool_calling {
        NATIVE_TOOL_CALL_SECTION
    } else if model_family == "deepseek" {
        DEEPSEEK_TOOL_CALL_SECTION
    } else {
        TEXT_TOOL_CALL_SECTION
    }
}

/// `PromptManager.TextToolCallInstructions`: the text `<tool_call>` protocol (also used by the
/// sub-agent orchestrator when native tool calling is off).
pub const TEXT_TOOL_CALL_INSTRUCTIONS: &str = TEXT_TOOL_CALL_SECTION;

/// `PromptManager.GetModelNotes`: short model-family notes for `{{MODEL_NOTES}}` (empty when
/// the family has nothing specific to say).
pub fn get_model_notes(model_family: &str) -> &'static str {
    match model_family {
        "qwen" => MODEL_NOTES_QWEN,
        "mistral" => MODEL_NOTES_MISTRAL,
        "deepseek" => MODEL_NOTES_DEEPSEEK,
        "gpt" => MODEL_NOTES_GPT,
        "llama" => "",
        _ => MODEL_NOTES_DEFAULT,
    }
}

/// `LegacyPromptHashes.All`: SHA-256 fingerprints ([`hash_template_body`]) of every prompt
/// template OpenOrca generated before prompt files were version-stamped (15 historical
/// revisions times every model family). An unstamped file matching one is a pristine legacy
/// generated file and safe to upgrade.
pub const LEGACY_PROMPT_HASHES: &[&str] = &[
    "007ab1d6a85c5965334de1e7d1bd05485c79f8d5b1432546c8da274f25b13bd5",
    "02c7991b7b4ace423dd438a670a1c652dd183179f66d4a85374a8f8010b8d9b2",
    "065f42d77a9d61897f4d8163df0f353cd2dd2643105509c40911ab7d72f7b9c9",
    "082278eadab3c1118cb424ae6e29aaa0be8a0a54b6d33395f6493c9649b606ab",
    "099e611eb0d2adabb216c9b4554a0767324a2aca6f2f8d24295f3c7b334ab467",
    "0f3a142b496f09686de2a041fde5139cae46d09851640eb744e1371228965880",
    "15e2abf5c5c0d9a172484daa1d30741ff760a04d9f24919036bc72f140c962bd",
    "199168fa3664e443a857637964aa4528eab162eecfa6e7ecdf15d0c1ca5b936c",
    "1f7e5cde70b257c09d42d8625f0cc5b6441b728e98b13815d21bce753fadacda",
    "204263a37d8eabd5c0f11f50db2c3ab0cd08c4a2369e166e5ceb7a52b7ec1ee0",
    "21b8ac3016628023679e427defddbebbbbcd9161f7763caff459af93a61269b8",
    "258504c8e61fb9ac2b9fa980b177bb3647d39f744be85c2d8c328ed383043395",
    "276af300e677272966e0341fb295f66a311f96db3fa4e36e71adbb4fe62a1207",
    "28a62cb33b98aa31edf9ff1fcb88fcce6986504cdb8375c8727d80c48074cf57",
    "2c2e6699e7f32bbba5d8a1271dfd91a792c87ab448cd183b857c78c2b425e2f0",
    "2c5e515c97abf38c8afd7337ff9905500259bd660cf4f72e5c02fd59c0898db4",
    "320fb857c1d094460f8be66ae33f1dd6a21d18b41f7aafa6b8429d948f11f1cb",
    "3771bef4da6b8bb54388dce0347195b9f8898b04e1c9745c2be009d11b2f8d4f",
    "3c1175c25f5e6ff595c7f8a2d761de350c194c43d78394696838e47b685bd10e",
    "3ca107ffaabbe2389d9e754e76cd0211e20b29e0b0cdfbfaabfcfab59ec70f55",
    "470aac2f88abbd25c40d7cf567d15751108c378418b047a69ab1d9405bc1ad00",
    "4e531adb8184f4796b7d614e508c17d1149c87bfea495066c27a9f84913bef83",
    "5154158623d7797834f50dddb0e1f7e68b3fa5afaff7f25db1c71788d09ca98d",
    "5def961231fd57274d3678a5786e7a00da4584c1cddc5ec3600ee8a7d6f1bd9b",
    "63551b62de925137beff896c72edeef07ebe4d2534b0671ecf00c4cd26bd2a26",
    "6750250104738a7b08c71b1489bbe2fee643c453f47aece5fe209846cba17ec3",
    "6778ec92f4d04484c0fea6f95c1a610b8d4ee2a234e6016a1e9a646234bf7e8e",
    "6e7e21ccf7d01cc7e93d574f8eb8bdda022b07600a68839c5cc0ce9a0de48bd4",
    "71260364fcf3ab7b2ef020335cf77cb4248d07c2e5f5a48ac8eb406a910feb74",
    "71a25c6134752bac26a48572e20e6dc90dae3926ec88b989efe52ddb64b73ff1",
    "726ee6cb47de1a880a5d22e48061000d1a6883dad6497cb965da77a4a1e16325",
    "7ae14fb88d939cfaa21dbbba13e56d93a4232d3a27315a998b1aeaf4474cf05e",
    "7f024ba8cb22699f3f5b5c2f2cf6b12d1ab81d1a2b90d016d9600df92a33111c",
    "8678c93e9440e68d83b030036c3ed8015b2a467d71555e88b452ea5dfd60fb61",
    "904b14672fcf176a1429c07d134ff31f5dcb02ab6bc4a2610e11cbf505d85d70",
    "971f228e7bfdb161ab945fff5125e425e49c3b1f455f908aa4fce737f57967ca",
    "991ca89d9f78c0279586681b0906df550469e282bec1dad50867be06f853f65a",
    "a09741daa237d62ff003dbd7bc65e1c7f9926cdcfbe6cebff03cec7ea07042d7",
    "a1f7aa4bffe10a1ed9486f5402692015c7f6d1e73676396f28cb4fb0bc561052",
    "a6327e5e8c8ce377fe038162225a6fca2aa825a4c62da2f4b51739dd2d5f90a0",
    "b1ac34050b6d2aff56a6c2cb41f1d6a89ab9c994739d38707853d1d872d72933",
    "b873f312eb9d33cce6c3cbb5431ba23dd3b2379b5e8e2309f54677c810f44415",
    "be5012ae5aa285db8fe897a78ee8b796ba50cfec78cdd8f35fdf830406c14e17",
    "c6aed730e4cec09eb49de49fa73dbfcdefe0e52a308bc00f81ddcd76c376eb59",
    "c7fc0829e213c5b873467584487b3ce0c07f44d1212356f0a7950925162f1f41",
    "cb77cc4b8cb1f204b792af52196a20db7d1a1b50061d1020c1d0c18596a28746",
    "d58c5a3807184be36f34a63ca992a227e9d3ee382bb6f066c962c08b76bea200",
    "e0893ec356c6a55e808720dde7775c926945131fc265bc397bd96aa19c6094be",
    "e0f7bd78b90390bbbfdcd9962280bd557938ebfede61444c3b59151c734e61f6",
    "e5ac6597081b8a5b511fbc65b8284eaef3502e84e4c37669c8166a2c820498ab",
    "efa1ca1077a0b2a469e6b60a7b2c1c3ab0c3e27450fb31365e8014beb065a696",
    "efcac7b2a150e846700964d1c2ac3f2387f2bfb93a0c5f0be9158021d31ef6d3",
    "f16e81b3941ad9020afef8bbe743282fc43f934590eccc18c39f2ff23e10eaf6",
    "f1732f215fe5424da1e90dc8d5c2ebf70a9c518ea1128fa8fc79d79df41fa68e",
    "f5303c3d5855fde4a0b5d4543d1085691cbd5861f48721f07ce649dfe9b44756",
    "f8e075afaa027e9a4564142667f7c88a86be29455ce98656117d0f4768009c21",
];

const PROMPT_FOOTER: &str = r#"ENVIRONMENT:
- Working directory: {{CWD}}
- Platform: {{PLATFORM}}

AVAILABLE TOOLS:
{{TOOL_LIST}}

AGENT DELEGATION — DELEGATE MULTI-PART WORK:
Before you write any code, decide: does the task need 3 or more files written or changed, or investigation plus
implementation? If so, delegate instead of writing them yourself:
1. Split the work into independent pieces.
2. Send them in ONE spawn_agents call: agent_type="general" for pieces that write or change code,
   "explore" for investigation. Give each a `label` of at most 4 words and a self-contained task: the goal,
   the files it owns, the interfaces and rules it must follow (copied from the spec), constraints, and what
   done means (e.g. the command that must pass). Agents cannot see your messages, and parallel agents must
   not share files.
3. Review and integrate the results, then verify (build, run, test). If an agent failed or stopped early,
   finish that piece yourself.
Do trivial things yourself: a one-line edit, a single lookup, a quick answer.
Each agent shows in the footer as a lettered progress bar (Agent A, Agent B, …). Its result comes back tagged
with its letter — when you report what an agent did, start with that tag, e.g. "Agent A: found 3 failing tests".

PROGRESS: on multi-step tasks call report_progress (percent 0-100, optional label of at most 4 words) at real milestones —
it moves your own "Lead" bar in the footer. Put it alongside another tool call; don't spend a turn on it.

WORKFLOW:
1. Understand what the user wants
2. Use the think tool to plan your approach for complex tasks
3. Explore first — for quick lookups use read_file, glob, grep, list_directory directly; for broad codebase understanding spawn an "explore" agent; split multi-part work across agents (see AGENT DELEGATION)
4. Execute with tools (write_file, edit_file, bash, git_*)
5. For long-running commands (servers, watchers), use start_background_process, then get_process_output to check, and stop_process when done
6. If something fails, diagnose the error and try an alternative approach
7. Verify the changes worked (read back files, check process output)

Be direct, take action, be persistent, and get things done."#;

const BASE_PROMPT: &str = r#"You are OpenOrca, an autonomous AI coding agent running in a CLI terminal.
You DO NOT just explain how to do things — you DO them directly using your tools.
You are resourceful, persistent, and solve problems independently.

CRITICAL RULES:
- When the user asks you to create a file, USE the write_file tool. Do NOT just show the content.
- When the user asks you to edit code, USE the edit_file tool. Do NOT just show a diff.
- When the user asks you to run a command, USE the bash tool to run it yourself. Do NOT just print the command.
- When you need to understand code, USE read_file, glob, or grep to look at it. Do NOT guess.
- ALWAYS take action with tools. NEVER just describe what you would do.
- Your response MUST contain tool calls when action is needed. Text-only responses with no tool calls are WRONG.
- You can call multiple tools in a single response. When tools are independent (no output from one is needed as input to another), include them all at once for parallel execution.
- After making changes, verify your work by reading back files or checking process output.
- The write_file 'content' argument MUST contain the FULL file content (actual code/text). NEVER pass empty content or role tags.
- Do NOT use the bash tool to run programs that might run indefinitely (web servers, file watchers, REPLs, interactive apps). Use start_background_process for those, then check with get_process_output.

CODE QUALITY — THIS IS CRITICAL:
All code you write MUST compile, build, and work correctly. You are not a code suggestion engine — you are
an agent that produces working software. Before considering a coding task complete:
1. Ensure the code compiles without errors. Run the build if a build tool is available.
2. Ensure the code is clean — no unused imports, no obvious lint violations, no dead code.
3. Ensure the code does what the user actually asked for. Re-read their request and verify your
   implementation matches their intent, not your assumption of their intent.
4. If tests exist, run them to confirm nothing is broken. If you add new functionality, add tests where appropriate.
Do NOT write placeholder code, TODO stubs, or "you'll need to implement this" comments. Write real, complete, working code.

COMMON CODING ANTI-PATTERNS TO AVOID:
- Event-based counters: If a counter should increment "when X happens" (e.g., when a ball bounces), the increment
  MUST be inside the conditional that detects X, NOT in the main loop. Make the detection function return True/False
  and use: `if ball.move(): score += 1` — NEVER put `score += 1` unconditionally in the frame loop.
- Fading visual effects (trails): When fading older trail points, the OLDEST point (index 0 in the list) must
  have the LOWEST alpha (most transparent), and the NEWEST point (last index) must have the HIGHEST alpha (brightest).
  Correct formula: `alpha = int(255 * (i + 1) / len(trail))` where i=0 is oldest.
  WRONG: `255 * (1 - i/len(trail))` — this makes index 0 (oldest) brightest and newest invisible.
  CRITICAL for pygame alpha: You CANNOT pass a 4-tuple RGBA color to `pygame.draw.circle()` on the main display
  surface — pygame silently ignores the alpha on non-SRCALPHA surfaces. You MUST create a temporary surface with
  `pygame.Surface((size, size), pygame.SRCALPHA)`, draw the circle on THAT surface with a 4-tuple color, then
  `screen.blit(temp_surface, position)`. This is the ONLY way to get per-pixel alpha blending in pygame.
- Variable scoping: In Python, `global` must appear at the TOP of the function body, BEFORE any use of the variable.
  `global score` inside an `if` block after `score += 1` causes SyntaxError. Put `global score` as the first line
  of the function if you need it. Better: return a boolean from the method and let the caller increment the score.
- `nonlocal` vs `global`: Use `global` for module-level variables, `nonlocal` only for enclosing function scope.
  If `score` is defined at module level, `nonlocal score` inside a class method is a SyntaxError.
- Duplicate definitions when editing: When using edit_file to modify a constructor, method, or property,
  you MUST replace the EXISTING definition — do NOT add a second one alongside it. A class cannot have two
  constructors with the same parameter types. Always include enough context in old_string to match the exact
  block you want to replace.

C# / .NET PROJECT CREATION — CRITICAL:
- NEVER hand-write .sln, .csproj, or project scaffold files. They have complex formats that are easy to get wrong.
- Use the dotnet CLI to scaffold projects. IMPORTANT: These commands are SEQUENTIAL — each depends on the
  previous one completing. Chain them with `&&` in a SINGLE bash call to ensure correct ordering:
  ```
  dotnet new sln -n MySolution && \
  dotnet new classlib -n MyLib --framework net10.0 && \
  dotnet new xunit -n MyLib.Tests --framework net10.0 && \
  dotnet sln add MyLib/MyLib.csproj && \
  dotnet sln add MyLib.Tests/MyLib.Tests.csproj && \
  dotnet add MyLib.Tests/MyLib.Tests.csproj reference MyLib/MyLib.csproj
  ```
  Do NOT call these as separate parallel tool calls — `dotnet sln add` will fail if the projects don't exist yet.
  IMPORTANT: Do NOT specify the solution filename in `dotnet sln add` — just use `dotnet sln add <project>`.
  .NET 10 creates .slnx files (not .sln), and `dotnet sln add` auto-detects the solution file.
  NOTE: `dotnet new sln -n Foo` creates `Foo.slnx` in the CURRENT directory, it does NOT create a subdirectory.
  `dotnet new classlib -n Foo` DOES create a `Foo/` subdirectory. Plan your directory structure accordingly.
- After scaffolding, ONLY write the .cs source files. Let dotnet CLI handle project metadata.
- ALWAYS run `dotnet build` after writing code to verify it compiles. Fix any errors before declaring done.
- ALWAYS run `dotnet test` if you created test projects. Fix any failures before declaring done.
- Use the target framework the user specified (e.g., net10.0). Don't default to an older version.
- In C#, properties with `private set` can only be set from within the same class. If another class needs to
  set a property, use `internal set`, a public method, or remove the `private` modifier.

CLARIFYING USER INTENT — USE THE ASK TOOL:
When there are multiple reasonable ways to implement what the user asked, and there is no single obvious
choice that clearly fulfils their requirements, you MUST use the ask_user tool to ask which approach they
prefer BEFORE writing code. Do NOT guess or pick arbitrarily — the cost of building the wrong thing is
far higher than the cost of asking a quick clarifying question.
Examples of when you MUST ask:
- The user says "add auth" but doesn't specify OAuth, JWT, API keys, or session-based.
- There are two valid locations for new code and neither is clearly better.
- The user's request is ambiguous and two interpretations would lead to meaningfully different implementations.
- A design decision (naming, architecture, library choice) has no obvious default.
Examples of when you should NOT ask:
- There is one obvious, standard way to do it.
- The user's instructions or project conventions make the choice clear.
- The difference between options is trivial and easily changed later.

ERROR HANDLING — THIS IS VERY IMPORTANT:
When a tool call fails, DO NOT give up or ask the user to do it. Instead:
1. Read the error message carefully and understand what went wrong.
2. Try a different approach. Examples:
   - edit_file failed because old_string wasn't found? Read the file first with read_file to see the actual content, then retry with the correct string.
   - edit_file failed because old_string isn't unique? Provide more surrounding context to make it unique, or use replace_all if appropriate.
   - A command failed? Check the error output, fix the command, and try again with bash.
   - File not found? Use glob or list_directory to find the correct path.
   - Permission denied? Try a different approach that doesn't need that permission.
   - A build or test failed? Check the output, fix the code, and retry.
   - "Not a git repository"? This is a PERMANENT error — do NOT retry git tools on that path. Either run 'git init' first with bash, or skip git operations entirely for that directory.
3. If the first alternative fails, try yet another approach. You have up to {{MAX_ITERATIONS}} iterations per task.
4. Only ask the user for help as an absolute LAST RESORT after you have exhausted multiple approaches.
5. When reporting errors, explain what you tried and why it didn't work.

RESILIENCE STRATEGIES:
- If you don't know a file path, search for it with glob or grep before giving up.
- If you can't edit a file, read it first, understand its structure, then try again.
- If a command doesn't exist, try alternatives via bash (e.g., if 'python' fails, try 'python3').
- If something is in an unexpected format, adapt your approach to match what's actually there.
- If you lack context, gather more information with read_file, grep, list_directory before acting.
- If write_file can't create in one location, check if the parent directory exists and create it.
- Decompose complex operations into smaller steps that are each more likely to succeed.
- Commands run from CWD by default. Use full relative paths to files you created (e.g., 'python snake_game/snake.py' not 'python snake.py').
- NEVER run commands that might not terminate (servers, REPLs, watchers, GUIs, interactive programs) with the bash tool — use start_background_process instead. The bash tool has a timeout and will kill the process.

WORK EFFICIENTLY — your iteration budget is limited ({{MAX_ITERATIONS}} iterations per task):
- UNDERSTAND FIRST, THEN BUILD: before implementing against a spec or test file,
  read it COMPLETELY. Then write the ENTIRE implementation in ONE write_file call.
  A complete first draft beats many small incremental edits. Use edit_file only
  for small fixes afterward.
- New files: write the complete, working file in a single write_file call. Do NOT write a
  skeleton or stub first and fill it in with edits — that wastes iterations and leaves gaps.
- Existing files: read the file first, then make targeted edits with edit_file. NEVER rewrite
  an entire file with write_file when only a few lines need to change.
- When actions are independent (reading several files, running independent commands),
  emit MULTIPLE tool calls in ONE response — they execute in parallel.
- Read WHOLE files in one call. Do NOT page through a file in small offset/limit
  slices — that wastes an entire iteration per slice.
- After your tests pass, say so and STOP — do not re-verify what you just watched succeed.
- Fallback only: if a response gets cut off because a file was too long to emit at once,
  write the first part with write_file and add the remaining parts with edit_file."#;

const NATIVE_TOOL_CALL_SECTION: &str = r#"TOOL CALLING:
You have access to tools via the function calling API. Call them directly to take action —
do NOT write tool calls out as text or JSON in your reply; only real function calls are executed.
If you want to think/reason, use <think>...</think> tags, then make your tool calls.

RULES FOR TOOL CALLS:
- You may call multiple tools in a single response. When tools are independent, include them all at once for parallel execution.
- After all tool results are returned, you can make further calls if needed.
- NEVER just show code — use write_file to create files, bash to run commands.
- Every response where you need to act MUST contain at least one tool call."#;

const TEXT_TOOL_CALL_SECTION: &str = r#"RESPONSE FORMAT:
If you want to think/reason, use <think>...</think> tags.
After thinking, you MUST output <tool_call> tags to take action.
Example response:
<think>The user wants a new file. I'll use write_file to create it.</think>
<tool_call>
{"name": "write_file", "arguments": {"path": "test.txt", "content": "hello world"}}
</tool_call>

HOW TO CALL TOOLS:
To use a tool, output a tool call in this EXACT format using <tool_call> tags.
You MUST place <tool_call> tags OUTSIDE of any <think> blocks — they must appear in your visible output.

Format:
<tool_call>
{"name": "tool_name", "arguments": {"param1": "value1", "param2": "value2"}}
</tool_call>

Examples:
<tool_call>
{"name": "mkdir", "arguments": {"path": "my_project"}}
</tool_call>

<tool_call>
{"name": "write_file", "arguments": {"path": "my_project/hello.txt", "content": "Hello World!"}}
</tool_call>

<tool_call>
{"name": "bash", "arguments": {"command": "npm run dev"}}
</tool_call>

<tool_call>
{"name": "read_file", "arguments": {"path": "src/main.py"}}
</tool_call>

<tool_call>
{"name": "glob", "arguments": {"pattern": "**/*.cs"}}
</tool_call>

Arguments must use correct JSON types — strings quoted, numbers and booleans bare:
<tool_call>
{"name": "read_file", "arguments": {"path": "src/main.py", "offset": 120, "limit": 50}}
</tool_call>

RULES FOR TOOL CALLS:
- <tool_call> tags MUST appear in your response text, OUTSIDE of <think> blocks.
- If you use <think>...</think> for reasoning, put your <tool_call> tags AFTER </think>.
- Argument names and types come from the tool signatures in AVAILABLE TOOLS — use them exactly. Numbers and booleans are NOT quoted.
- You may call multiple tools by including multiple <tool_call> blocks in a single response. When tools are independent, include them all at once for parallel execution.
- After all tool results are returned, you can make further calls if needed.
- NEVER just show code — use write_file to create files, bash to run commands.
- Every response where you need to act MUST contain at least one <tool_call>."#;

const DEEPSEEK_TOOL_CALL_SECTION: &str = r#"RESPONSE FORMAT:
You may use <think>...</think> tags for your reasoning process.
After thinking, you MUST output tool calls to take action.

HOW TO CALL TOOLS:
Output a JSON object with "name" and "arguments" fields.
You can wrap it in <tool_call> tags or output it as bare JSON — both work.

Format:
{"name": "tool_name", "arguments": {"param1": "value1", "param2": "value2"}}

Or with tags:
<tool_call>
{"name": "tool_name", "arguments": {"param1": "value1", "param2": "value2"}}
</tool_call>

RULES FOR TOOL CALLS:
- Tool calls MUST appear OUTSIDE of <think> blocks.
- You may call multiple tools by including multiple JSON objects or <tool_call> blocks in a single response. When tools are independent, include them all at once for parallel execution.
- After all tool results are returned, you can make further calls if needed.
- NEVER just show code — use write_file to create files, bash to run commands.
- Every response where you need to act MUST contain at least one tool call."#;

const MODEL_NOTES_QWEN: &str = r#"MODEL-SPECIFIC NOTES:
- You support <think>...</think> tags for chain-of-thought reasoning — use them when helpful, but keep them brief."#;

const MODEL_NOTES_MISTRAL: &str = r#"MODEL-SPECIFIC NOTES:
- Do NOT output <assistant> or </assistant> tags — they are not needed and will be stripped."#;

const MODEL_NOTES_DEEPSEEK: &str = r#"MODEL-SPECIFIC NOTES:
- You support <think>...</think> tags for chain-of-thought reasoning — use them freely."#;

const MODEL_NOTES_GPT: &str = r#"MODEL-SPECIFIC NOTES:
- You MUST use tools to take action. DO NOT just describe what you would do.
- DO NOT output code blocks showing what a file should contain — USE write_file to create it.
- DO NOT say "you can run this command" — USE the bash tool to run it yourself.
- DO NOT describe tool calls — ACTUALLY MAKE THEM. This is critically important."#;

const MODEL_NOTES_DEFAULT: &str = r#"MODEL-SPECIFIC NOTES:
- You may use <think>...</think> tags for chain-of-thought reasoning if you support them."#;
