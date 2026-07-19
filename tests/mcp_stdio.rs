use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const MESSAGE_TIMEOUT: Duration = Duration::from_secs(15);

struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
}

impl McpSession {
    fn spawn() -> Self {
        Self::spawn_with_env(&[])
    }

    fn spawn_with_env(environment: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pix-mcp"));
        command.env("RUST_LOG", "error");
        for name in [
            "PIX_MCP_CAPTURES_DIR",
            "PIX_MCP_INPUT_ROOTS",
            "PIX_MCP_OUTPUT_ROOTS",
            "PIX_MCP_EXECUTABLE_ROOTS",
            "PIX_MCP_ALLOW_UNC_PATHS",
            "PIX_MCP_ALLOW_ELEVATED_LAUNCH",
            "PIX_MCP_MAX_CONCURRENT_TOOLS",
        ] {
            command.env_remove(name);
        }
        for (name, value) in environment {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start pix-mcp test server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message = match line {
                    Ok(line) if line.trim().is_empty() => continue,
                    Ok(line) => serde_json::from_str(&line)
                        .map_err(|error| format!("invalid server JSON {line:?}: {error}")),
                    Err(error) => Err(format!("failed to read server stdout: {error}")),
                };
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            messages,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("server stdin is open");
        serde_json::to_writer(&mut *stdin, &message).expect("serialize client message");
        stdin.write_all(b"\n").expect("write message terminator");
        stdin.flush().expect("flush client message");
    }

    fn receive(&self) -> Value {
        self.messages
            .recv_timeout(MESSAGE_TIMEOUT)
            .expect("server response timed out")
            .unwrap_or_else(|error| panic!("server transport error: {error}"))
    }

    fn receive_until(&self, mut predicate: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + MESSAGE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self
                .messages
                .recv_timeout(remaining)
                .expect("matching server response timed out")
                .unwrap_or_else(|error| panic!("server transport error: {error}"));
            if predicate(&message) {
                return message;
            }
        }
    }
}

fn initialize(session: &mut McpSession) {
    session.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "pix-mcp-integration-test", "version": "1" }
        }
    }));
    let response = session.receive_until(|message| message["id"] == json!(1));
    assert!(response.get("error").is_none(), "{response}");
    session.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    }));
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_task_fallback_and_elicitation_cancellation_are_protocol_compliant() {
    let mut session = McpSession::spawn();
    session.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": { "elicitation": { "form": {} } },
            "clientInfo": { "name": "pix-mcp-integration-test", "version": "1" }
        }
    }));
    let initialize = session.receive();
    assert_eq!(initialize["id"], json!(1));
    assert_eq!(initialize["result"]["protocolVersion"], "2025-11-25");
    assert!(initialize["result"]["capabilities"]["tasks"].is_null());
    assert!(initialize["result"]["capabilities"]["resources"]["listChanged"].is_null());
    session.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    }));

    session.send(json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list", "params": {} }));
    let tools = session.receive_until(|message| message["id"] == json!(6));
    let listed_tools = tools["result"]["tools"].as_array().expect("tool list");
    assert!(!listed_tools.is_empty());
    for tool in listed_tools {
        assert_eq!(
            tool["inputSchema"]["additionalProperties"],
            json!(false),
            "tool schema must be closed: {tool}"
        );
    }

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "pix_list_captures",
            "arguments": { "unexpected": true }
        }
    }));
    let strict_input = session.receive_until(|message| message["id"] == json!(7));
    let strict_text = strict_input.to_string().to_ascii_lowercase();
    assert!(
        strict_input.get("error").is_some() || strict_input["result"]["isError"] == json!(true),
        "unknown input unexpectedly succeeded: {strict_input}"
    );
    assert!(
        strict_text.contains("unknown field") || strict_text.contains("invalid"),
        "unknown-field error was not actionable: {strict_input}"
    );

    // MCP requires task metadata to be ignored when the server does not
    // advertise task execution. rmcp 2.2 otherwise rejects this request.
    session.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "pix_status", "arguments": {}, "task": {} }
    }));
    let task_call = session.receive_until(|message| message["id"] == json!(2));
    assert!(task_call.get("error").is_none(), "{task_call}");
    assert!(task_call["result"]["structuredContent"].is_object());

    // Discovery is deterministic, annotated, and rejects cursors for
    // unpaginated endpoints rather than silently returning duplicate pages.
    session.send(json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/list",
        "params": {}
    }));
    let tools = session.receive_until(|message| message["id"] == json!(20));
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .expect("tool array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 21,
        "method": "tools/list",
        "params": { "cursor": "not-a-tool-cursor" }
    }));
    let bad_tool_cursor = session.receive_until(|message| message["id"] == json!(21));
    assert_eq!(bad_tool_cursor["error"]["code"], json!(-32602));

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 22,
        "method": "resources/list",
        "params": {}
    }));
    let resources = session.receive_until(|message| message["id"] == json!(22));
    assert_eq!(resources["result"]["resources"][0]["uri"], "capture://list");
    assert_eq!(
        resources["result"]["resources"][0]["annotations"]["audience"][0],
        "assistant"
    );

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 23,
        "method": "resources/list",
        "params": { "cursor": "not-a-resource-cursor" }
    }));
    let bad_resource_cursor = session.receive_until(|message| message["id"] == json!(23));
    assert_eq!(bad_resource_cursor["error"]["code"], json!(-32602));

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 24,
        "method": "resources/read",
        "params": { "uri": "https://example.test/not-a-pix-resource" }
    }));
    let invalid_resource = session.receive_until(|message| message["id"] == json!(24));
    assert_eq!(invalid_resource["error"]["code"], json!(-32602));

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 25,
        "method": "resources/read",
        "params": { "uri": "capture://definitely-missing-pix-mcp-test" }
    }));
    let missing_resource = session.receive_until(|message| message["id"] == json!(25));
    assert_eq!(missing_resource["error"]["code"], json!(-32002));

    // A matching progress token produces optional MCP progress even if the
    // operation then fails as a normal, caller-visible tool error.
    session.send(json!({
        "jsonrpc": "2.0",
        "id": 26,
        "method": "tools/call",
        "params": {
            "name": "pix_run_analysis",
            "arguments": { "capture_path": "definitely-missing.wpix" },
            "_meta": { "progressToken": "pix-progress-test" }
        }
    }));
    let progress = session.receive_until(|message| {
        message["method"] == "notifications/progress"
            && message["params"]["progressToken"] == "pix-progress-test"
    });
    assert_eq!(progress["params"]["progress"], json!(0.0));
    let progressed_call = session.receive_until(|message| message["id"] == json!(26));
    assert_eq!(progressed_call["result"]["isError"], json!(true));

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "pix_get_screenshot",
            "arguments": { "capture_path": "missing.wpix" }
        }
    }));
    let elicitation = session.receive_until(|message| message["method"] == "elicitation/create");
    let elicitation_id = elicitation["id"].clone();
    session.send(json!({
        "jsonrpc": "2.0",
        "id": elicitation_id,
        "result": { "action": "decline" }
    }));
    let declined = session.receive_until(|message| message["id"] == json!(3));
    assert_eq!(declined["result"]["isError"], json!(true));
    assert!(
        declined["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("elicitation was declined")),
        "{declined}"
    );

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "pix_get_screenshot",
            "arguments": { "capture_path": "missing.wpix" }
        }
    }));
    let elicitation = session.receive_until(|message| message["method"] == "elicitation/create");
    let elicitation_id = elicitation["id"].clone();
    session.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 4, "reason": "integration test" }
    }));
    let child_cancel = session.receive_until(|message| {
        if message["id"] == json!(4) {
            panic!("cancelled parent request unexpectedly produced a response: {message}");
        }
        message["method"] == "notifications/cancelled"
    });
    assert_eq!(child_cancel["params"]["requestId"], elicitation_id);

    session.send(json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" }));
    let ping = session.receive_until(|message| {
        if message["id"] == json!(4) {
            panic!("cancelled parent request unexpectedly produced a response: {message}");
        }
        message["id"] == json!(5)
    });
    assert!(ping.get("error").is_none(), "{ping}");
}

#[test]
fn stdio_security_policy_uses_capture_directory_and_enforces_roots() {
    let captures = tempfile::tempdir().expect("captures directory");
    let outside = tempfile::tempdir().expect("outside directory");
    std::fs::write(captures.path().join("allowed.wpix"), b"capture").expect("capture fixture");
    let outside_csv = outside.path().join("outside.csv");
    std::fs::write(&outside_csv, b"Counter,Value\nGPU,1\n").expect("counter fixture");

    let captures_text = captures.path().to_string_lossy().into_owned();
    let mut session = McpSession::spawn_with_env(&[
        ("PIX_MCP_CAPTURES_DIR", &captures_text),
        ("PIX_MCP_INPUT_ROOTS", ""),
        ("PIX_MCP_OUTPUT_ROOTS", ""),
    ]);
    initialize(&mut session);

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "pix_list_captures", "arguments": {} }
    }));
    let listed = session.receive_until(|message| message["id"] == json!(2));
    assert_eq!(listed["result"]["structuredContent"]["total_count"], 1);
    assert_eq!(
        listed["result"]["structuredContent"]["captures"][0]["name"],
        "allowed.wpix"
    );

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "pix_export_counters",
            "arguments": { "file_path": outside_csv.to_string_lossy() }
        }
    }));
    let denied_input = session.receive_until(|message| message["id"] == json!(3));
    assert_eq!(denied_input["result"]["isError"], true, "{denied_input}");
    assert!(
        denied_input
            .to_string()
            .contains("outside the configured allowlist"),
        "{denied_input}"
    );

    session.send(json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "pix_gpu_capture",
            "arguments": {
                "process_id": 1,
                "output_path": outside.path().join("denied.wpix").to_string_lossy()
            }
        }
    }));
    let denied_output = session.receive_until(|message| message["id"] == json!(4));
    assert_eq!(denied_output["result"]["isError"], true, "{denied_output}");
    assert!(
        denied_output
            .to_string()
            .contains("outside the configured allowlist"),
        "{denied_output}"
    );
}
