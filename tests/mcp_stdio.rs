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
        let mut child = Command::new(env!("CARGO_BIN_EXE_pix-mcp"))
            .env("RUST_LOG", "error")
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
    session.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    }));

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
