// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! Streaming client for the OpenAI Responses API.
//!
//! The request runs on a plain worker thread with a blocking HTTP client and
//! forwards parsed SSE deltas over an async channel; the UI side drains the
//! channel on the GLib main context. Dropping the receiver (closing the
//! window) makes the next send fail, which is how a stream is cancelled.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{json, Value};

use crate::tools::ToolRegistry;

pub enum ChatEvent {
    /// A chunk of the visible answer text.
    Delta(String),
    /// A chunk of the model's reasoning, when the stream carries one. Whether a
    /// trace appears is left to the model/endpoint; the client just renders it.
    /// Display-only — never sent back as history.
    Reasoning(String),
    /// A tool is about to run. `name` captions the spinner; `item` is the full
    /// `function_call` JSON that must be persisted into the conversation history
    /// (the UI mirrors it into its `history` so follow-up turns include it).
    ToolCall { name: String, item: Value },
    /// A tool finished. `item` is the `function_call_output` JSON to persist
    /// right after its matching call.
    ToolResult { item: Value },
    Done,
    Error(String),
}

#[derive(Clone)]
pub struct ApiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// GET `{base_url}/models` on a worker thread and send the sorted model ids
/// to `sender`. Returns immediately.
pub fn list_models(api: ApiConfig, sender: async_channel::Sender<Result<Vec<String>, String>>) {
    std::thread::spawn(move || {
        let url = format!("{}/models", api.base_url.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        let result = agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", api.api_key))
            .call()
            .map_err(|err| match err {
                ureq::Error::Status(code, response) => status_error_message(code, response),
                err => err.to_string(),
            })
            .and_then(|response| {
                let body: Value = response
                    .into_json()
                    .map_err(|err| format!("invalid response: {err}"))?;
                let mut models: Vec<String> = body["data"]
                    .as_array()
                    .map(|models| {
                        models
                            .iter()
                            .filter_map(|m| m["id"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                models.sort();
                Ok(models)
            });
        let _ = sender.send_blocking(result);
    });
}

/// Best-effort `error.message` from an HTTP error response body, falling
/// back to the bare status code.
fn status_error_message(code: u16, response: ureq::Response) -> String {
    let body = response.into_string().unwrap_or_default();
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {code}"))
}

/// POST `{base_url}/responses` with `stream: true` on a worker thread,
/// forwarding events to `sender`. Returns immediately.
///
/// When `tools` is non-empty the request advertises them and this runs a
/// multi-turn agent loop: the model's `function_call`s are executed here and
/// their outputs fed back, repeating until the model produces a final answer.
pub fn stream_chat(
    api: ApiConfig,
    messages: Vec<Value>,
    tools: ToolRegistry,
    sender: async_channel::Sender<ChatEvent>,
) {
    std::thread::spawn(move || {
        let event = run(&api, messages, &tools, &sender);
        let _ = sender.send_blocking(event);
    });
}

/// Cap on agent-loop iterations so a model that keeps calling tools without
/// ever answering can't spin the worker thread forever.
const MAX_TURNS: usize = 8;

/// Drive the conversation to a final answer, owning the growing `input` vec.
/// Each iteration runs one model turn; if it asked for tools, they are executed
/// and their results appended before the next turn.
fn run(
    api: &ApiConfig,
    mut input: Vec<Value>,
    tools: &ToolRegistry,
    sender: &async_channel::Sender<ChatEvent>,
) -> ChatEvent {
    for _turn in 0..MAX_TURNS {
        let calls = match run_turn(api, &input, tools, sender) {
            Ok(TurnOutcome::Completed) => return ChatEvent::Done,
            Ok(TurnOutcome::ToolCalls(calls)) => calls,
            Err(event) => return event,
        };

        for call in calls {
            let name = call["name"].as_str().unwrap_or_default().to_string();
            let call_id = call["call_id"].as_str().unwrap_or_default().to_string();
            // `arguments` is a JSON-encoded string; pass it through verbatim.
            let args_str = call["arguments"].as_str().unwrap_or("{}").to_string();

            let call_item = json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": args_str,
            });
            if sender
                .send_blocking(ChatEvent::ToolCall { name: name.clone(), item: call_item.clone() })
                .is_err()
            {
                return ChatEvent::Done; // receiver dropped: window closed
            }
            input.push(call_item);

            // Malformed arguments are reported back to the model as the output
            // so it can retry, rather than aborting the loop.
            let parsed = serde_json::from_str::<Value>(&args_str).unwrap_or_else(|_| json!({}));
            let output = match tools.dispatch(&name, parsed) {
                Ok(out) => out,
                Err(err) => format!("error: {err}"),
            };

            let output_item = json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            });
            if sender
                .send_blocking(ChatEvent::ToolResult { item: output_item.clone() })
                .is_err()
            {
                return ChatEvent::Done;
            }
            input.push(output_item);
        }
    }
    ChatEvent::Error("The assistant kept calling tools without finishing".to_string())
}

/// The result of a single model turn.
enum TurnOutcome {
    /// The model produced only text: this is the final answer.
    Completed,
    /// The model emitted one or more `function_call` items to execute.
    ToolCalls(Vec<Value>),
}

/// Run one model turn: POST `/responses`, stream text deltas as they arrive,
/// and collect any `function_call` items the model emits. Terminal failures
/// (HTTP errors, a dropped receiver, a truncated stream) are returned as
/// `Err(ChatEvent::…)` for the caller to forward.
fn run_turn(
    api: &ApiConfig,
    input: &[Value],
    tools: &ToolRegistry,
    sender: &async_channel::Sender<ChatEvent>,
) -> Result<TurnOutcome, ChatEvent> {
    let url = format!("{}/responses", api.base_url.trim_end_matches('/'));
    let mut body = json!({
        "model": api.model,
        "stream": true,
        "input": input,
        // The conversation is resent in full each turn; no server-side state.
        "store": false,
    });
    if let Some(definitions) = tools.definitions() {
        body["tools"] = definitions;
    }

    // No overall timeout (the response is an open-ended stream), but bound
    // each read so a stalled server can't hang the worker thread forever.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(120))
        .build();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", api.api_key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());

    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            return Err(ChatEvent::Error(status_error_message(code, response)));
        }
        Err(err) => return Err(ChatEvent::Error(err.to_string())),
    };

    // Forward an event to the UI; a failed send means the receiver was dropped
    // (the window closed), so the caller should stop streaming.
    let send = |event| sender.send_blocking(event).is_ok();
    // Reasoning summaries arrive in one or more parts; once any reasoning has
    // been forwarded, a fresh part is separated from the previous with a gap.
    let mut reasoning_seen = false;

    let reader = BufReader::new(response.into_reader());
    let mut calls: Vec<Value> = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => return Err(ChatEvent::Error(format!("Stream interrupted: {err}"))),
        };
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            continue;
        };
        if data == "[DONE]" {
            // Some endpoints close with a bare [DONE]; treat it as turn end.
            return Ok(turn_outcome(calls));
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        // Responses API events carry their type in the payload; only the text
        // deltas, reasoning summaries, tool calls, terminal states, and errors
        // matter here.
        match chunk["type"].as_str() {
            Some("response.output_text.delta") => {
                if let Some(delta) = chunk["delta"].as_str() {
                    if !send(ChatEvent::Delta(delta.to_string())) {
                        // Receiver dropped: the window was closed, stop streaming.
                        return Err(ChatEvent::Done);
                    }
                }
            }
            // Summarised thinking (`reasoning.summary`) and, for models that
            // expose it, the raw reasoning text — both shown as a trace.
            Some("response.reasoning_summary_text.delta")
            | Some("response.reasoning_text.delta") => {
                if let Some(delta) = chunk["delta"].as_str() {
                    reasoning_seen = true;
                    if !send(ChatEvent::Reasoning(delta.to_string())) {
                        return Err(ChatEvent::Done);
                    }
                }
            }
            // A new summary part: separate it from the previous one with a gap.
            Some("response.reasoning_summary_part.added") if reasoning_seen => {
                if !send(ChatEvent::Reasoning("\n\n".to_string())) {
                    return Err(ChatEvent::Done);
                }
            }
            // A fully-formed output item: harvest function calls here, where the
            // item carries name, call_id and the complete arguments string in
            // one place (the most provider-portable harvest point).
            Some("response.output_item.done") => {
                if chunk["item"]["type"] == "function_call" {
                    calls.push(chunk["item"].clone());
                }
            }
            Some("response.completed") => return Ok(turn_outcome(calls)),
            Some("response.failed") | Some("response.incomplete") => {
                // Failed responses carry `error.message`; incomplete ones
                // carry `incomplete_details.reason` (e.g. max_output_tokens).
                let message = chunk["response"]["error"]["message"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| {
                        chunk["response"]["incomplete_details"]["reason"]
                            .as_str()
                            .map(|reason| format!("Response incomplete: {reason}"))
                    })
                    .unwrap_or_else(|| "The response did not complete".to_string());
                return Err(ChatEvent::Error(message));
            }
            Some("error") => {
                let message = chunk["message"].as_str().unwrap_or("Stream error").to_string();
                return Err(ChatEvent::Error(message));
            }
            _ => {}
        }
    }
    // EOF without a terminal event: the connection was cut mid-response.
    Err(ChatEvent::Error("The stream ended unexpectedly".to_string()))
}

/// A turn with no collected calls is the final answer; otherwise its calls run.
fn turn_outcome(calls: Vec<Value>) -> TurnOutcome {
    if calls.is_empty() {
        TurnOutcome::Completed
    } else {
        TurnOutcome::ToolCalls(calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve `body` as a close-delimited SSE response to one connection and
    /// return the base URL to point the client at.
    fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = stream.write_all(response.as_bytes());
        });
        format!("http://{addr}")
    }

    /// Serve `bodies` as close-delimited SSE responses, one per incoming
    /// connection (so a multi-turn agent loop sees a fresh response each turn),
    /// and return the base URL.
    fn serve_each(bodies: Vec<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn tool_call_runs_and_continues_the_conversation() {
        // Turn 1 asks to run a tool; turn 2 (after the result is fed back) gives
        // the final answer.
        let url = serve_each(vec![
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"true\\\"}\"}}\n\n\
             data: {\"type\":\"response.completed\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"all done\"}\n\n\
             data: {\"type\":\"response.completed\"}\n\n",
        ]);
        let api = ApiConfig {
            base_url: url,
            api_key: String::new(),
            model: "test".to_string(),
        };
        let tools = crate::tools::registry_for(&["bash".to_string()]);
        let (sender, receiver) = async_channel::unbounded();
        let terminal = run(&api, Vec::new(), &tools, &sender);
        drop(sender);

        let mut deltas = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = 0;
        while let Ok(event) = receiver.recv_blocking() {
            match event {
                ChatEvent::Delta(delta) => deltas.push_str(&delta),
                ChatEvent::ToolCall { name, .. } => tool_calls.push(name),
                ChatEvent::ToolResult { .. } => tool_results += 1,
                _ => {}
            }
        }
        assert_eq!(tool_calls, vec!["bash".to_string()]);
        assert_eq!(tool_results, 1);
        assert_eq!(deltas, "all done");
        assert!(matches!(terminal, ChatEvent::Done));
    }

    /// Run a stream against `base_url` and return (concatenated deltas,
    /// terminal event).
    fn run_stream(base_url: String) -> (String, ChatEvent) {
        let api = ApiConfig {
            base_url,
            api_key: String::new(),
            model: "test".to_string(),
        };
        let (sender, receiver) = async_channel::unbounded();
        let terminal = run(&api, Vec::new(), &ToolRegistry::new(), &sender);
        drop(sender);
        let mut deltas = String::new();
        while let Ok(event) = receiver.recv_blocking() {
            if let ChatEvent::Delta(delta) = event {
                deltas.push_str(&delta);
            }
        }
        (deltas, terminal)
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let url = serve_once(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        );
        let (deltas, terminal) = run_stream(url);
        assert_eq!(deltas, "partial");
        match terminal {
            ChatEvent::Error(message) => assert_eq!(message, "The stream ended unexpectedly"),
            _ => panic!("expected an error for a truncated stream"),
        }
    }

    #[test]
    fn reasoning_deltas_are_forwarded_separately_from_the_answer() {
        let url = serve_once(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\n\
             data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
             data: {\"type\":\"response.completed\"}\n\n",
        );
        let api = ApiConfig {
            base_url: url,
            api_key: String::new(),
            model: "test".to_string(),
        };
        let (sender, receiver) = async_channel::unbounded();
        let terminal = run(&api, Vec::new(), &ToolRegistry::new(), &sender);
        drop(sender);
        let (mut reasoning, mut answer) = (String::new(), String::new());
        while let Ok(event) = receiver.recv_blocking() {
            match event {
                ChatEvent::Reasoning(delta) => reasoning.push_str(&delta),
                ChatEvent::Delta(delta) => answer.push_str(&delta),
                _ => {}
            }
        }
        assert_eq!(reasoning, "think");
        assert_eq!(answer, "hi");
        assert!(matches!(terminal, ChatEvent::Done));
    }

    #[test]
    fn completed_stream_is_done() {
        let url = serve_once(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
             data: {\"type\":\"response.completed\"}\n\n",
        );
        let (deltas, terminal) = run_stream(url);
        assert_eq!(deltas, "hi");
        assert!(matches!(terminal, ChatEvent::Done));
    }

    #[test]
    fn incomplete_response_reports_the_reason() {
        let url = serve_once(
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        );
        let (_, terminal) = run_stream(url);
        match terminal {
            ChatEvent::Error(message) => {
                assert_eq!(message, "Response incomplete: max_output_tokens")
            }
            _ => panic!("expected an error for an incomplete response"),
        }
    }

    #[test]
    fn http_error_surfaces_the_server_message() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = "{\"error\":{\"message\":\"Invalid API key\"}}";
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let (_, terminal) = run_stream(format!("http://{addr}"));
        match terminal {
            ChatEvent::Error(message) => assert_eq!(message, "Invalid API key"),
            _ => panic!("expected an error for an HTTP 401"),
        }
    }
}
