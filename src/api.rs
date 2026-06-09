// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 breitburg

//! Streaming client for OpenAI-compatible chat completion endpoints.
//!
//! The request runs on a plain worker thread with a blocking HTTP client and
//! forwards parsed SSE deltas over an async channel; the UI side drains the
//! channel on the GLib main context. Dropping the receiver (closing the
//! window) makes the next send fail, which is how a stream is cancelled.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{json, Value};

pub enum ChatEvent {
    Delta(String),
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
                ureq::Error::Status(code, _) => format!("HTTP {code}"),
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

/// POST `{base_url}/chat/completions` with `stream: true` on a worker thread,
/// forwarding events to `sender`. Returns immediately.
pub fn stream_chat(api: ApiConfig, messages: Vec<Value>, sender: async_channel::Sender<ChatEvent>) {
    std::thread::spawn(move || {
        let event = run(&api, &messages, &sender);
        let _ = sender.send_blocking(event);
    });
}

fn run(api: &ApiConfig, messages: &[Value], sender: &async_channel::Sender<ChatEvent>) -> ChatEvent {
    let url = format!("{}/chat/completions", api.base_url.trim_end_matches('/'));
    let body = json!({
        "model": api.model,
        "stream": true,
        "messages": messages,
    });

    // Connect timeout only: the response is an open-ended stream.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .build();
    let response = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", api.api_key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());

    let response = match response {
        Ok(response) => response,
        Err(ureq::Error::Status(code, response)) => {
            let body = response.into_string().unwrap_or_default();
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("HTTP {code}"));
            return ChatEvent::Error(message);
        }
        Err(err) => return ChatEvent::Error(err.to_string()),
    };

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
        // Role-only first chunk and the final chunk carry no content; skip.
        if let Some(content) = chunk["choices"][0]["delta"]["content"].as_str() {
            if sender.send_blocking(ChatEvent::Delta(content.to_string())).is_err() {
                // Receiver dropped: the window was closed, stop streaming.
                return ChatEvent::Done;
            }
        }
    }
    ChatEvent::Done
}
