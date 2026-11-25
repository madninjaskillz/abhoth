//! Ported from `PromptManagerTests.cs`, plus parity checks against the C# build.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_core::config::prompt_manager::{
    CURRENT_PROMPT_VERSION, DEFAULT_PROMPT_FILE_NAME, LEGACY_PROMPT_HASHES, PromptFileStatus,
    PromptManager, classify_template, detect_model_family, generate_default_template,
    get_model_notes, get_tool_protocol_section, hash_template_body, shipped_template_body, slugify,
};

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    warnings: Arc<Mutex<Vec<String>>>,
    manager: PromptManager,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // The prompts dir does not exist yet (as in C#: Path.Combine(temp, "openorca-prompts-…")).
        let dir = tmp.path().join("prompts");
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let manager = PromptManager::with_dir(&dir).record_warnings(warnings.clone());
        Self {
            _tmp: tmp,
            dir,
            warnings,
            manager,
        }
    }

    fn default_path(&self) -> PathBuf {
        self.dir.join(DEFAULT_PROMPT_FILE_NAME)
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::create_dir_all(&self.dir).unwrap();
        std::fs::write(self.dir.join(name), content).unwrap();
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn legacy_prompt() -> String {
    read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-default-prompt.md"))
}

fn vars(native: bool, family: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "TOOL_PROTOCOL",
            get_tool_protocol_section(native, family).to_string(),
        ),
        ("MODEL_NOTES", get_model_notes(family).to_string()),
        ("MAX_ITERATIONS", "42".into()),
        ("CWD", "/work".into()),
        ("PLATFORM", "test-os".into()),
        ("TOOL_LIST", "  - bash".into()),
    ]
}

// --- Protocol section follows the effective mode ---

#[test]
fn native_protocol_never_teaches_text_tool_call_tags() {
    for family in ["default", "qwen", "deepseek", "mistral"] {
        let section = get_tool_protocol_section(true, family);
        assert!(section.contains("function calling API"));
        assert!(!section.contains("<tool_call>"));
    }
}

#[test]
fn text_protocol_teaches_tool_call_tags() {
    let section = get_tool_protocol_section(false, "default");
    assert!(section.contains("<tool_call>"));
    assert!(!section.contains("function calling API"));
}

#[test]
fn model_notes_never_advise_skeleton_first() {
    for family in ["default", "qwen", "llama", "mistral", "deepseek", "gpt"] {
        assert!(!get_model_notes(family).to_lowercase().contains("skeleton"));
    }
}

#[test]
fn shipped_template_has_runtime_placeholders_and_no_stale_guidance() {
    let body = shipped_template_body();
    assert!(body.contains("{{TOOL_PROTOCOL}}"));
    assert!(body.contains("{{MAX_ITERATIONS}}"));
    assert!(body.contains("UNDERSTAND FIRST, THEN BUILD"));
    assert!(!body.contains("<tool_call>"));
    assert!(!body.contains("25 iterations"));
    assert!(!body.contains("write a concise skeleton first"));
    assert!(!body.contains("~200 lines"));
}

#[test]
fn load_no_files_uses_shipped_template_with_native_protocol() {
    let f = Fixture::new();
    let prompt = f
        .manager
        .load_system_prompt(None, "qwen3-coder", &vars(true, "qwen"));
    assert!(prompt.contains("UNDERSTAND FIRST, THEN BUILD"));
    assert!(prompt.contains("function calling API"));
    assert!(!prompt.contains("<tool_call>"));
    assert!(prompt.contains("up to 42 iterations"));
    assert!(!prompt.contains("{{"));
}

#[test]
fn load_same_generated_file_switches_protocol_with_mode() {
    let f = Fixture::new();
    f.manager.ensure_default_prompt().unwrap();

    let native = f
        .manager
        .load_system_prompt(None, "", &vars(true, "default"));
    let text = f
        .manager
        .load_system_prompt(None, "", &vars(false, "default"));

    assert!(!native.contains("<tool_call>"));
    assert!(text.contains("<tool_call>"));
    assert!(!native.contains("<!--")); // header comments are not sent to the model
}

// --- Generated prompt upgrade vs user-edit preservation ---

#[test]
fn ensure_creates_stamped_current_file() {
    let f = Fixture::new();
    let status = f.manager.ensure_default_prompt().unwrap();

    assert_eq!(status, PromptFileStatus::Missing);
    let content = read(&f.default_path());
    assert!(content.starts_with(&format!(
        "<!-- openorca-prompt v{CURRENT_PROMPT_VERSION} sha256:"
    )));
    assert_eq!(classify_template(&content), PromptFileStatus::Current);
    assert_eq!(
        f.manager.ensure_default_prompt().unwrap(),
        PromptFileStatus::Current
    );
}

#[test]
fn classify_crlf_copy_of_generated_file_is_still_unedited() {
    let crlf = generate_default_template().replace('\n', "\r\n");
    assert_eq!(classify_template(&crlf), PromptFileStatus::Current);
}

#[test]
fn ensure_upgrades_legacy_unedited_generated_file() {
    let f = Fixture::new();
    let legacy = legacy_prompt();
    f.write(DEFAULT_PROMPT_FILE_NAME, &legacy);
    assert_eq!(
        classify_template(&legacy),
        PromptFileStatus::LegacyGenerated
    );

    let status = f.manager.ensure_default_prompt().unwrap();

    assert_eq!(status, PromptFileStatus::LegacyGenerated);
    let upgraded = read(&f.default_path());
    assert_eq!(upgraded, generate_default_template());
    assert!(!upgraded.contains("write a concise skeleton first"));
}

#[test]
fn ensure_upgrades_stale_stamped_file() {
    // An unedited file generated by an older stamped version: valid stamp, older body.
    let f = Fixture::new();
    let old_body = "You are OpenOrca (old shipped prompt).\nwrite a concise skeleton first";
    let stale = format!(
        "<!-- openorca-prompt v1 sha256:{} -->\n\n{old_body}\n",
        hash_template_body(old_body)
    );
    f.write(DEFAULT_PROMPT_FILE_NAME, &stale);

    let status = f.manager.ensure_default_prompt().unwrap();

    assert_eq!(status, PromptFileStatus::StaleGenerated);
    assert_eq!(read(&f.default_path()), generate_default_template());
}

#[test]
fn ensure_preserves_user_edited_stamped_file_and_warns() {
    let f = Fixture::new();
    let edited = generate_default_template() + "\nALWAYS answer in French.\n";
    f.write(DEFAULT_PROMPT_FILE_NAME, &edited);

    let status = f.manager.ensure_default_prompt().unwrap();

    assert_eq!(status, PromptFileStatus::UserEdited);
    assert_eq!(read(&f.default_path()), edited);
    assert_eq!(f.warnings.lock().unwrap().len(), 1);
}

#[test]
fn ensure_preserves_edited_legacy_file() {
    let f = Fixture::new();
    let edited = legacy_prompt().replace("You are OpenOrca", "You are MyOrca");
    f.write(DEFAULT_PROMPT_FILE_NAME, &edited);

    assert_eq!(
        f.manager.ensure_default_prompt().unwrap(),
        PromptFileStatus::UserEdited
    );
    assert_eq!(read(&f.default_path()), edited);
}

#[test]
fn load_user_edited_file_is_used_verbatim() {
    let f = Fixture::new();
    f.write(
        DEFAULT_PROMPT_FILE_NAME,
        "<!-- mine -->\nCustom prompt in {{CWD}}.\n",
    );
    let prompt = f
        .manager
        .load_system_prompt(None, "", &vars(true, "default"));
    assert_eq!(prompt, "Custom prompt in /work.");
}

#[test]
fn load_legacy_generated_per_model_file_falls_through_to_default() {
    let f = Fixture::new();
    f.write(DEFAULT_PROMPT_FILE_NAME, "My customized default prompt.");
    f.write(&format!("{}.md", slugify("some/model")), &legacy_prompt());

    let prompt = f
        .manager
        .load_system_prompt(None, "some/model", &vars(true, "default"));

    assert_eq!(prompt, "My customized default prompt.");
}

#[test]
fn load_legacy_generated_default_uses_shipped_template() {
    let f = Fixture::new();
    f.write(DEFAULT_PROMPT_FILE_NAME, &legacy_prompt());

    let prompt = f
        .manager
        .load_system_prompt(None, "", &vars(true, "default"));

    assert!(prompt.contains("UNDERSTAND FIRST, THEN BUILD"));
    assert!(!prompt.contains("<tool_call>"));
}

// --- Rust-only: parity with the C# build and resolution order ---

/// The stamp the C# build writes into `prompts/default.md` (v3; checked byte for byte against a
/// file written by the C# Release build, 2026-11-25). Update both when the shipped prompt changes.
const CSHARP_SHIPPED_BODY_HASH: &str =
    "381a8da35d7d21cf52c312ce4560bffcab9e3b7808fd031a5cbe120cdad9a3ed";

#[test]
fn shipped_body_hash_matches_csharp() {
    assert_eq!(
        hash_template_body(shipped_template_body()),
        CSHARP_SHIPPED_BODY_HASH
    );
    assert!(generate_default_template().starts_with(&format!(
        "<!-- openorca-prompt v3 sha256:{CSHARP_SHIPPED_BODY_HASH} -->\n"
    )));
    assert_eq!(LEGACY_PROMPT_HASHES.len(), 56);
}

#[test]
fn load_profile_then_model_then_default() {
    let f = Fixture::new();
    f.write(DEFAULT_PROMPT_FILE_NAME, "default prompt");
    f.write("qwen-qwen3-32b.md", "model prompt");
    f.write(
        "mine.md",
        "<!-- header -->\r\n\r\nprofile prompt\r\n\r\n\r\n\r\nend  ",
    );
    let v = vars(true, "qwen");
    assert_eq!(
        f.manager
            .load_system_prompt(Some("mine"), "qwen/qwen3-32b", &v),
        "profile prompt\n\nend"
    );
    assert_eq!(
        f.manager.load_system_prompt(None, "qwen/qwen3-32b", &v),
        "model prompt"
    );
    // A missing profile warns and falls through.
    assert_eq!(
        f.manager.load_system_prompt(Some("nope"), "other", &v),
        "default prompt"
    );
    assert_eq!(f.warnings.lock().unwrap().len(), 1);
}

#[test]
fn slugify_and_family() {
    assert_eq!(slugify(" Qwen/Qwen3-32B "), "qwen-qwen3-32b");
    assert_eq!(slugify("a__b..c--"), "a-b-c");
    assert_eq!(detect_model_family("Qwen3-Coder"), "qwen");
    assert_eq!(detect_model_family("meta-llama-3"), "llama");
    assert_eq!(detect_model_family("deepseek-r1"), "deepseek");
    assert_eq!(detect_model_family("my-r1-distill"), "deepseek");
    assert_eq!(detect_model_family("gpt-oss-20b"), "gpt");
    assert_eq!(detect_model_family("fake-model"), "default");
    assert_eq!(get_model_notes("llama"), "");
}
