//! Ports of `ClaudeBridgeModesTests.cs`: host tools (ask_user over the in-process MCP server),
//! /ask turns (no tools, same session) and plan mode (the CLI's plan permission mode).

use serde_json::{Value, json};

use super::tests::create;
use super::*;
use crate::chat::Conversation;
use crate::client::Role;
use crate::engines::test_support::*;

const SID: &str = "00000000-0000-4000-8000-000000000001";

fn init() -> String {
    json!({"type": "system", "subtype": "init", "session_id": SID, "model": "claude-haiku", "cwd": "C:\\work\\proj"})
        .to_string()
}

fn text(id: &str, text: &str) -> String {
    json!({
        "type": "assistant",
        "message": {"id": id, "role": "assistant", "content": [{"type": "text", "text": text}]},
        "parent_tool_use_id": null,
        "session_id": SID,
    })
    .to_string()
}

fn result(text: &str) -> String {
    json!({
        "type": "result", "subtype": "success", "is_error": false, "result": text, "session_id": SID,
        "total_cost_usd": 0.001, "num_turns": 1, "duration_ms": 5,
        "usage": {"input_tokens": 10, "output_tokens": 2},
    })
    .to_string()
}

fn mcp(request_id: &str, message: Value) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "mcp_message", "server_name": "openorca", "message": message},
    })
    .to_string()
}

fn can_use(request_id: &str, tool: &str, input: Value) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input, "tool_use_id": format!("toolu_{request_id}")},
    })
    .to_string()
}

fn mcp_response(fake: &FakeClaude, request_id: &str) -> Value {
    let r: Vec<Value> = fake
        .written_of_type("control_response", None)
        .into_iter()
        .filter(|r| r["response"]["request_id"] == request_id)
        .collect();
    assert_eq!(r.len(), 1);
    r[0]["response"]["response"]["mcp_response"].clone()
}

fn arg_after(args: &[String], flag: &str) -> String {
    let i = args
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("{flag}"));
    args[i + 1].clone()
}

#[tokio::test]
async fn ask_user_served_over_in_process_mcp_answer_goes_back_to_the_cli() {
    // Haiku sends AskUserQuestion's shape (as a string): normalized to {question, options}.
    let ask_args = json!({
        "questions": "[{\"question\":\"Favourite colour?\",\"options\":[{\"label\":\"Red\"},{\"label\":\"Purple\"}]}]",
    });
    let turn = vec![
        init(),
        mcp(
            "m2",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        ),
        mcp(
            "m3",
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "ask_user", "arguments": ask_args}}),
        ),
        text("msg1", "COLOUR=Purple"),
        result("COLOUR=Purple"),
    ];
    let options = ClaudeBridgeOptions {
        host_tools: vec![ask_user_spec()],
        ..Default::default()
    };
    let mut h = create(vec![turn], Some(options), |s| {
        s.emit_on_initialize.push(mcp(
            "m0",
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}}),
        ));
        s.emit_on_initialize.push(mcp(
            "m1",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ));
    });
    let mut sink = RecordingSink::new(true);
    let mut conversation = Conversation::new();
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "Ask me my favourite colour", &mut sink),
            &CancelToken::new(),
        )
        .await
        .unwrap();

    // Launched with the host server declared, ask_user pre-allowed and AskUserQuestion off.
    let args = h.start_args(0);
    assert!(args.contains(&"--mcp-config".to_string()));
    assert_eq!(
        arg_after(&args, "--allowedTools"),
        "mcp__openorca__ask_user"
    );
    assert_eq!(arg_after(&args, "--disallowedTools"), "AskUserQuestion");
    assert!(args.contains(&ASK_USER_PROMPT.to_string()));
    let mcp_config: Value =
        serde_json::from_str(&std::fs::read_to_string(arg_after(&args, "--mcp-config")).unwrap())
            .unwrap();
    assert_eq!(mcp_config["mcpServers"]["openorca"]["type"], "sdk");
    let fake = h.fake(0);
    let init = &fake.written_of_type("control_request", Some("initialize"))[0];
    assert_eq!(init["request"]["sdkMcpServers"][0], "openorca");

    // MCP handshake (answered during startup), tool list, then the call through OpenOrca's ask_user.
    assert_eq!(
        mcp_response(&fake, "m0")["result"]["protocolVersion"],
        "2025-11-25"
    );
    let tools = &mcp_response(&fake, "m2")["result"]["tools"];
    assert_eq!(tools[0]["name"], "ask_user");
    let calls = sink.host_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ask_user");
    let a: Value = serde_json::from_str(&calls[0].1).unwrap();
    assert_eq!(a["question"], "Favourite colour?");
    assert_eq!(a["options"], json!(["Red", "Purple"]));
    let answer = &mcp_response(&fake, "m3")["result"];
    assert_eq!(answer["content"][0]["text"], "Purple");
    assert_eq!(answer["isError"], false);
    // The notification got its empty answer.
    assert_eq!(
        mcp_response(&fake, "m1"),
        json!({"jsonrpc": "2.0", "result": {}, "id": 0})
    );
}

#[tokio::test]
async fn ask_turn_restarts_without_tools_resuming_the_session_then_back_to_normal() {
    let turns = vec![
        vec![init(), text("m1", "one"), result("one")],
        vec![init(), text("m2", "two"), result("two")],
        vec![init(), text("m3", "three"), result("three")],
    ];
    let options = ClaudeBridgeOptions {
        host_tools: vec![ask_user_spec()],
        ..Default::default()
    };
    let mut h = create(turns, Some(options), |_| {});
    let mut conversation = Conversation::new();
    let mut sink = RecordingSink::new(true);
    let ct = CancelToken::new();

    h.engine
        .run_turn(AgentTurn::new(&mut conversation, "first", &mut sink), &ct)
        .await
        .unwrap();
    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "explain", &mut sink).with_mode(AgentTurnMode::Ask),
            &ct,
        )
        .await
        .unwrap();
    h.engine
        .run_turn(AgentTurn::new(&mut conversation, "third", &mut sink), &ct)
        .await
        .unwrap();

    assert_eq!(h.start_count(), 3);
    let ask = h.start_args(1);
    assert_eq!(arg_after(&ask, "--tools"), "");
    assert!(ask.contains(&"--strict-mcp-config".to_string()));
    assert!(!ask.contains(&"--mcp-config".to_string()));
    assert_eq!(arg_after(&ask, "--resume"), SID);
    let back = h.start_args(2);
    assert!(!back.contains(&"--tools".to_string()));
    assert!(back.contains(&"--mcp-config".to_string()));
    assert_eq!(arg_after(&back, "--resume"), SID);
    assert!(h.fake(0).disposed());
    let answers: Vec<String> = conversation
        .messages()
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert_eq!(answers, ["one", "two", "three"]);
}

fn plan_file() -> String {
    let config = std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| paths::combine(&user_profile(), ".claude"));
    paths::combine(&paths::combine(&config, "plans"), "a-plan.md")
}

#[test]
fn plan_files_are_recognised() {
    assert!(is_plan_file(&plan_file()));
    let elsewhere = paths::combine(
        &paths::combine(&std::env::temp_dir().to_string_lossy(), "plans"),
        "a-plan.md",
    );
    assert!(!is_plan_file(&elsewhere));
}

#[tokio::test]
async fn plan_turn_sets_plan_permission_mode_exit_plan_mode_becomes_the_answer() {
    let plan = "1. Add the flag\n2. Test it";
    let turns = vec![
        vec![
            init(),
            can_use(
                "w1",
                "Write",
                json!({"file_path": plan_file(), "content": plan}),
            ),
            can_use("p1", "ExitPlanMode", json!({"plan": plan})),
            result(""),
        ],
        vec![init(), text("m2", "done"), result("done")],
    ];
    let mut h = create(turns, None, |_| {});
    let mut conversation = Conversation::new();
    let mut sink = RecordingSink::new(true);
    let ct = CancelToken::new();

    h.engine
        .run_turn(
            AgentTurn::new(&mut conversation, "plan it", &mut sink).with_mode(AgentTurnMode::Plan),
            &ct,
        )
        .await
        .unwrap();

    let written = h.fake(0).written();
    let mode_at = written
        .iter()
        .position(|w| {
            w["type"] == "control_request" && w["request"]["subtype"] == "set_permission_mode"
        })
        .expect("set_permission_mode sent");
    assert_eq!(written[mode_at]["request"]["mode"], "plan");
    assert!(mode_at < written.iter().position(|w| w["type"] == "user").unwrap());
    // The CLI's plan file is written without a prompt; ExitPlanMode is refused (OpenOrca asks
    // the user to approve the plan after the turn), never prompted.
    assert!(sink.approvals().is_empty());
    let behaviors: Vec<Value> = h
        .fake(0)
        .written_of_type("control_response", None)
        .iter()
        .map(|r| r["response"]["response"]["behavior"].clone())
        .collect();
    assert_eq!(behaviors, [json!("allow"), json!("deny")]);
    assert!(
        sink.events()
            .contains(&AgentEngineEvent::TextDelta(plan.into()))
    );
    let done = sink.completed();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].final_text.as_deref(), Some(plan));
    assert_eq!(
        conversation.messages().last().unwrap().content.as_deref(),
        Some(plan)
    );

    // Next normal turn switches the mode back.
    h.engine
        .run_turn(AgentTurn::new(&mut conversation, "go", &mut sink), &ct)
        .await
        .unwrap();
    let modes: Vec<Value> = h
        .fake(0)
        .written_of_type("control_request", Some("set_permission_mode"))
        .iter()
        .map(|w| w["request"]["mode"].clone())
        .collect();
    assert_eq!(modes, [json!("plan"), json!("default")]);
    assert_eq!(h.fake_count(), 1);
}
