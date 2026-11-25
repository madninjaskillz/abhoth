//! Port of `PairModelVoterTests` (vote parsing), plus the vote prompt.

use orca_core::agent::pair_vote::{
    build_vote_prompt, parse_vote_choice, parse_vote_choice_ab, truncate_model_name,
};
use orca_core::client::ChatResponse;

fn create_response(text: &str) -> ChatResponse {
    ChatResponse {
        text: text.into(),
        ..ChatResponse::default()
    }
}

#[test]
fn parse_vote_choice_json_choice_a_returns_a() {
    let response = create_response("{\"choice\":\"A\"}");
    assert_eq!(parse_vote_choice_ab(&response), Some("A"));
}

#[test]
fn parse_vote_choice_json_choice_b_returns_b() {
    let response = create_response("{\"choice\":\"B\"}");
    assert_eq!(parse_vote_choice_ab(&response), Some("B"));
}

#[test]
fn parse_vote_choice_with_think_block_strips_and_parses_json() {
    let response = create_response("<think>Hmm, B is better</think>{\"choice\":\"B\"}");
    assert_eq!(parse_vote_choice_ab(&response), Some("B"));
}

#[test]
fn parse_vote_choice_fallback_text_matching_finds_a() {
    let response = create_response("I choose \"A\" because it's better.");
    assert_eq!(parse_vote_choice_ab(&response), Some("A"));
}

#[test]
fn parse_vote_choice_no_choice_found_returns_null() {
    let response = create_response("I'm not sure which is better.");
    assert_eq!(parse_vote_choice_ab(&response), None);
}

#[test]
fn parse_vote_choice_lowercase_choice_normalizes_to_upper() {
    let response = create_response("{\"choice\":\"b\"}");
    assert_eq!(parse_vote_choice_ab(&response), Some("B"));
}

// ---- beyond the C# tests ----

#[test]
fn parse_vote_choice_rejects_a_letter_past_the_choices_and_falls_back() {
    // "C" is not one of two choices; the quoted-letter fallback finds no "A" or "B" either.
    assert_eq!(
        parse_vote_choice(&create_response("{\"choice\":\"C\"}"), 2),
        None
    );
    assert_eq!(
        parse_vote_choice(&create_response("{\"choice\":\"C\"}"), 3),
        Some(2)
    );
    // A non-string choice falls through to the text match.
    assert_eq!(
        parse_vote_choice(&create_response("{\"choice\":1} \"B\""), 2),
        Some(1)
    );
}

#[test]
fn vote_prompt_labels_answers_and_lists_the_choices() {
    let valid = vec![("first".to_string(), 0), ("second".to_string(), 2)];
    assert_eq!(
        build_vote_prompt("do it", &valid),
        "The user said: do it\n\nResponse A:\nfirst\n\nResponse B:\nsecond\n\nWhich response better addresses the user's request? Reply with ONLY valid JSON and nothing else: {\"choice\":\"A\"}, {\"choice\":\"B\"}"
    );
}

#[test]
fn vote_prompt_cuts_long_answers() {
    let long = "x".repeat(1200);
    let valid = vec![(long, 0), ("short".to_string(), 1)];
    let prompt = build_vote_prompt("u", &valid);
    // max(500, 2000 / 2) = 1000 units, then "...".
    assert!(prompt.contains(&format!("Response A:\n{}...\n\n", "x".repeat(1000))));
}

#[test]
fn truncate_model_name_keeps_the_part_after_the_last_slash() {
    assert_eq!(
        truncate_model_name("meta-llama/llama-4-maverick-17b-128e-instruct"),
        "llama-4-maverick-17b-128e-instruct"
    );
    assert_eq!(truncate_model_name("plain"), "plain");
}
