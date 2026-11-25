//! Recognises "the prompt no longer fits the context window" errors across providers
//! (`ContextOverflow.cs`), so the agent can compact and retry instead of failing over or
//! giving up. Patterns follow Pi's overflow.ts plus LiteLLM's wrapper error name.

use std::error::Error;
use std::sync::LazyLock;

use regex::Regex;

static OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?is)prompt (?:is )?too long",           // Anthropic, z.ai
        r"|prompt exceeds max length",             // z.ai CN
        r"|request_too_large",                     // Anthropic 413
        r"|input is too long for requested model", // Bedrock
        r"|exceeds the context window",            // OpenAI
        r"|exceeds (?:the )?(?:model'?s )?maximum context length", // OpenAI-compatible proxies
        r"|input token count.*exceeds the maximum", // Gemini
        r"|maximum prompt length is \d+",          // xAI
        r"|reduce the length of the messages",     // Groq
        r"|maximum context length is \d+ tokens",  // OpenRouter, vLLM
        r"|exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
        r"|input \(\d+ tokens\) is longer than the model'?s context length", // Together
        r"|exceeds the available context size",    // llama.cpp server
        r"|greater than the context length",       // LM Studio
        r"|context window exceeds limit",          // MiniMax
        r"|exceeded model token limit",            // Kimi
        r"|too large for model with \d+ maximum context length", // Mistral
        r"|prompt has [\d,]+ tokens?, but the configured context size is [\d,]+", // DS4
        r"|model_context_window_exceeded",         // z.ai finish reason
        r"|prompt too long; exceeded (?:max )?context length", // Ollama
        r"|range of input length should be",       // DashScope
        r"|ContextWindowExceeded",                 // LiteLLM error class
        r"|context[_ ]length[_ ]exceeded",         // OpenAI code, generic
        r"|token limit exceeded",                  // generic
    ))
    .unwrap()
});

// Rate limiting / throttling can mention "too many tokens" etc.: never treat as overflow.
static NON_OVERFLOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)rate limit|too many requests|^(?:Throttling error|Service unavailable):")
        .unwrap()
});

// The server's real window, when the error states it.
static CONTEXT_SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)available context size \(([\d,]+) tokens?\)",
        r"|maximum context length is ([\d,]+) tokens?",
        r"|model'?s context length \(([\d,]+) tokens?\)",
        r"|configured context size is ([\d,]+) tokens?",
    ))
    .unwrap()
});

/// Whether `message` reports a context overflow.
pub fn is_overflow(message: &str) -> bool {
    !message.is_empty() && OVERFLOW.is_match(message) && !NON_OVERFLOW.is_match(message)
}

/// Whether the error or any error in its source chain reports a context overflow.
pub fn is_overflow_error(err: &(dyn Error + 'static)) -> bool {
    chain(err).any(|m| is_overflow(&m))
}

/// The context size stated in `message`.
pub fn try_parse_context_size(message: &str) -> Option<i32> {
    let caps = CONTEXT_SIZE.captures(message)?;
    let digits = caps
        .iter()
        .skip(1)
        .flatten()
        .next()?
        .as_str()
        .replace(',', "");
    digits.parse::<i32>().ok().filter(|n| *n > 0)
}

/// The context size the server reported in the error (or its sources), when it states one.
pub fn try_parse_context_size_error(err: &(dyn Error + 'static)) -> Option<i32> {
    chain(err).find_map(|m| try_parse_context_size(&m))
}

fn chain<'a>(err: &'a (dyn Error + 'static)) -> impl Iterator<Item = String> + 'a {
    std::iter::successors(Some(err), |&e: &&'a (dyn Error + 'static)| e.source())
        .map(|e| e.to_string())
}
