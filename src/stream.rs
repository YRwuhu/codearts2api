//! 上游 SSE / JSON 响应解析。
//!
//! CodeArts 有两种反常行为必须兼容：
//! - 请求 stream=false 时也可能返回 text/event-stream；
//! - 失败时可能返回 HTTP 200，把 error_code 塞在 JSON 或 SSE 帧里。

use serde_json::{json, Map, Value};

use crate::params::{is_business_error, normalize_finish_reason};

/// 把 SSE 原始字节聚合成一个 OpenAI chat.completion 对象。
pub fn parse_sse_bytes(raw: &[u8]) -> Value {
    let text = String::from_utf8_lossy(raw);
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Map<String, Value> = Map::new();
    let mut usage: Option<Value> = None;
    let mut last_finish: Option<String> = None;

    let mut result = json!({
        "id": Value::Null,
        "object": "chat.completion",
        "created": Value::Null,
        "model": Value::Null,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": ""},
            "finish_reason": "stop"
        }],
    });

    for line in text.lines() {
        let line = line.trim();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            continue;
        };

        if is_business_error(&chunk) {
            return crate::params::error_payload(&chunk);
        }

        for key in ["id", "created", "model"] {
            if let Some(v) = chunk.get(key) {
                if !v.is_null() {
                    result[key] = v.clone();
                }
            }
        }
        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                usage = Some(u.clone());
            }
        }

        let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        for choice in choices {
            if let Some(reason) = choice.get("finish_reason") {
                if !reason.is_null() {
                    last_finish = Some(normalize_finish_reason(Some(reason)));
                }
            }
            let Some(delta) = choice.get("delta") else { continue };

            if let Some(s) = delta.get("content").and_then(|v| v.as_str()) {
                text_parts.push(s.to_string());
            }
            if let Some(s) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                reasoning_parts.push(s.to_string());
            }

            let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
                continue;
            };
            for call in calls {
                let index = call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                let key = index.to_string();
                let entry = tool_calls.entry(key).or_insert_with(|| {
                    json!({
                        "id": Value::Null,
                        "type": "function",
                        "function": {"name": "", "arguments": ""}
                    })
                });
                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        entry["id"] = json!(id);
                    }
                }
                if let Some(t) = call.get("type").and_then(|v| v.as_str()) {
                    entry["type"] = json!(t);
                }
                if let Some(func) = call.get("function") {
                    if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                        let prev = entry["function"]["name"].as_str().unwrap_or("");
                        entry["function"]["name"] = json!(format!("{prev}{name}"));
                    }
                    if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                        let prev = entry["function"]["arguments"].as_str().unwrap_or("");
                        entry["function"]["arguments"] = json!(format!("{prev}{args}"));
                    }
                }
            }
        }
    }

    let content = text_parts.join("");
    let reasoning = reasoning_parts.join("");
    let has_tools = !tool_calls.is_empty();

    if let Some(choices) = result.get_mut("choices").and_then(|c| c.as_array_mut()) {
        if let Some(choice) = choices.first_mut() {
            let message = &mut choice["message"];
            message["content"] = json!(content);
            if !reasoning.is_empty() {
                message["reasoning_content"] = json!(reasoning);
            }
            if has_tools {
                let mut sorted: Vec<(i64, Value)> = tool_calls
                    .iter()
                    .map(|(k, v)| (k.parse::<i64>().unwrap_or(0), v.clone()))
                    .collect();
                sorted.sort_by_key(|(k, _)| *k);
                message["tool_calls"] = Value::Array(sorted.into_iter().map(|(_, v)| v).collect());
            }
            choice["finish_reason"] = json!(last_finish.unwrap_or_else(|| {
                if has_tools {
                    "tool_calls".to_string()
                } else {
                    "stop".to_string()
                }
            }));
        }
    }

    if let Some(u) = usage {
        result["usage"] = u;
    }
    result
}

/// 兼容上游返回普通 JSON 或错误地返回 SSE 的情况。
pub fn parse_upstream(raw: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(raw) {
        Ok(mut v) => {
            if let Some(choices) = v.get_mut("choices").and_then(|c| c.as_array_mut()) {
                for choice in choices.iter_mut() {
                    let fr = normalize_finish_reason(choice.get("finish_reason"));
                    choice["finish_reason"] = json!(fr);
                }
            }
            v
        }
        Err(_) => parse_sse_bytes(raw),
    }
}

/// 把一行上游 SSE 转发成客户端可用的帧。
/// 返回 `None` 表示该行无需转发（心跳、空行等）。
pub fn forward_stream_line(raw_line: &[u8]) -> StreamAction {
    let line = String::from_utf8_lossy(raw_line);
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return StreamAction::Skip;
    }
    if trimmed.starts_with(':') {
        return StreamAction::Passthrough(trimmed.to_string());
    }
    let Some(payload) = trimmed.strip_prefix("data:") else {
        return StreamAction::Skip;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return StreamAction::Skip;
    }
    if payload == "[DONE]" {
        return StreamAction::Done;
    }
    let Ok(mut chunk) = serde_json::from_str::<Value>(payload) else {
        return StreamAction::Skip;
    };

    if is_business_error(&chunk) {
        return StreamAction::Error(crate::params::error_payload(&chunk));
    }

    if let Some(choices) = chunk.get_mut("choices").and_then(|c| c.as_array_mut()) {
        for choice in choices {
            if let Some(reason) = choice.get("finish_reason") {
                if reason.as_str() == Some("other") {
                    choice["finish_reason"] = json!("stop");
                }
            }
            // 部分上游发 content:null；只认字符串 content 的客户端会当成无响应。
            if let Some(delta) = choice.get_mut("delta") {
                if let Some(content) = delta.get("content") {
                    if content.is_null() {
                        delta["content"] = json!("");
                    }
                }
            }
        }
    }

    StreamAction::Chunk(chunk)
}

pub enum StreamAction {
    Skip,
    Done,
    Passthrough(String),
    Chunk(Value),
    Error(Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_text_deltas() {
        let raw = br#"data: {"id":"x","choices":[{"delta":{"content":"he"}}]}
data: {"id":"x","choices":[{"delta":{"content":"llo"},"finish_reason":"stop"}]}
data: [DONE]
"#;
        let v = parse_sse_bytes(raw);
        assert_eq!(v["choices"][0]["message"]["content"], "hello");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn detects_error_inside_sse() {
        let raw = br#"data: {"error_code":"InferHub.001001005.400","error_msg":"bad param"}
"#;
        let v = parse_sse_bytes(raw);
        assert!(v.get("error").is_some());
        assert_eq!(v["error"]["code"], "InferHub.001001005.400");
    }

    #[test]
    fn aggregates_tool_call_fragments() {
        let raw = br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"Ba","arguments":"{\"a\""}}]}}]}
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"sh","arguments":":1}"}}]},"finish_reason":"tool_calls"}]}
"#;
        let v = parse_sse_bytes(raw);
        assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "Bash");
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
    }

    #[test]
    fn other_finish_reason_becomes_stop() {
        let v = parse_upstream(br#"{"choices":[{"message":{"content":"x"},"finish_reason":"other"}]}"#);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn null_content_becomes_empty_string() {
        match forward_stream_line(br#"data: {"choices":[{"delta":{"content":null}}]}"#) {
            StreamAction::Chunk(v) => assert_eq!(v["choices"][0]["delta"]["content"], ""),
            _ => panic!("expected chunk"),
        }
    }
}
