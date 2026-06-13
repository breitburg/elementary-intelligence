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

pub enum ChatEvent {
    /// A chunk of the visible answer text.
    Delta(String),
    /// A chunk of the model's reasoning, when the stream carries one. Whether a
    /// trace appears is left to the model/endpoint; the client just renders it.
    /// Display-only — never sent back as history.
    Reasoning(String),
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
pub fn stream_chat(api: ApiConfig, messages: Vec<Value>, sender: async_channel::Sender<ChatEvent>) {
    std::thread::spawn(move || {
        let event = run(&api, &messages, &sender);
        let _ = sender.send_blocking(event);
    });
}

fn run(api: &ApiConfig, messages: &[Value], sender: &async_channel::Sender<ChatEvent>) -> ChatEvent {
    let url = format!("{}/responses", api.base_url.trim_end_matches('/'));
    let body = json!({
        "model": api.model,
        "stream": true,
        "input": messages,
        // The conversation is resent in full each turn; no server-side state.
        "store": false,
    });

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
            return ChatEvent::Error(status_error_message(code, response));
        }
        Err(err) => return ChatEvent::Error(err.to_string()),
    };

    // Forward an event to the UI; a failed send means the receiver was dropped
    // (the window closed), so the caller should stop streaming.
    let send = |event| sender.send_blocking(event).is_ok();
    // Reasoning summaries arrive in one or more parts; once any reasoning has
    // been forwarded, a fresh part is separated from the previous with a gap.
    let mut reasoning_seen = false;

    let reader = BufReader::new(response.into_reader());
    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => return ChatEvent::Error(format!("Stream interrupted: {err}")),
        };
        let Some(data) = line.strip_prefix("data:").map(str::trim_start) else {
            continue;
        };
        if data == "[DONE]" {
            return ChatEvent::Done;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        // Responses API events carry their type in the payload; only the text
        // deltas, reasoning summaries, terminal states, and errors matter here.
        match chunk["type"].as_str() {
            Some("response.output_text.delta") => {
                if let Some(delta) = chunk["delta"].as_str() {
                    if !send(ChatEvent::Delta(delta.to_string())) {
                        return ChatEvent::Done;
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
                        return ChatEvent::Done;
                    }
                }
            }
            // A new summary part: separate it from the previous one with a gap.
            Some("response.reasoning_summary_part.added") if reasoning_seen => {
                if !send(ChatEvent::Reasoning("\n\n".to_string())) {
                    return ChatEvent::Done;
                }
            }
            Some("response.completed") => return ChatEvent::Done,
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
                return ChatEvent::Error(message);
            }
            Some("error") => {
                let message = chunk["message"].as_str().unwrap_or("Stream error").to_string();
                return ChatEvent::Error(message);
            }
            _ => {}
        }
    }
    // EOF without a terminal event: the connection was cut mid-response.
    ChatEvent::Error("The stream ended unexpectedly".to_string())
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

    /// Run a stream against `base_url` and return (concatenated deltas,
    /// terminal event).
    fn run_stream(base_url: String) -> (String, ChatEvent) {
        let api = ApiConfig {
            base_url,
            api_key: String::new(),
            model: "test".to_string(),
        };
        let (sender, receiver) = async_channel::unbounded();
        let terminal = run(&api, &[], &sender);
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
        let terminal = run(&api, &[], &sender);
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
