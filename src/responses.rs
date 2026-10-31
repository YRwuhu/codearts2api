//! OpenAI **Responses API** (`POST /v1/responses`) ←→ Chat Completions 互转。
//!
//! Responses 是 Codex / 新版 OpenAI SDK 使用的格式，与 Chat Completions 的
//! 主要差异：
//!
//! - 输入是 `input`（字符串或 item 数组），不是 `messages`
//! - `instructions` 与首条 `system` 分开
//! - 输出是 `output` 数组（`reasoning` / `message` / `function_call` item）
//! - 流式事件是 `response.output_item.added` / `response.output_text.delta` /
//!   `response.completed` 等语义化事件，不是裸 delta
//! - `max_output_tokens` 替代 `max_tokens`
//!
//! 上游只认 Chat Completions，所以这里做双向翻译。

use axum::response::Response;
use serde_json::{json, Value};

use crate::chat::{ApiError, EventStream, StreamEvent};

/// Responses 请求 → OpenAI chat 请求。
pub fn to_chat_request(req: &Value) -> Result<Value, ApiError> {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(instructions) = req.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.trim().is_empty() {
            messages.push(json!({"role": "system", "content": instructions}));
        }
    }

    match req.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role": "user", "content": text}));
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(m) = input_item_to_message(item)? {
                    messages.push(m);
                }
            }
        }
        None => {
            return Err(ApiError::bad_request("缺少 input 字段"));
        }
        Some(_) => {
            return Err(ApiError::bad_request("input 必须是字符串或数组"));
        }
    }

    if messages.is_empty() {
        return Err(ApiError::bad_request("input 中没有任何可用消息"));
    }

    let mut out = json!({
        "model": req.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
    });

    if let Some(v) = req.get("max_output_tokens") {
        out["max_tokens"] = v.clone();
    } else if let Some(v) = req.get("max_tokens") {
        out["max_tokens"] = v.clone();
    }
    for key in ["temperature", "top_p", "stream", "user", "metadata"] {
        if let Some(v) = req.get(key) {
            if key == "metadata" {
                continue; // 上游不认
            }
            out[key] = v.clone();
        }
    }
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let converted: Vec<Value> = tools.iter().filter_map(responses_tool_to_chat).collect();
        if !converted.is_empty() {
            out["tools"] = Value::Array(converted);
            if let Some(tc) = req.get("tool_choice") {
                out["tool_choice"] = tc.clone();
            }
            // 并行工具调用
            if let Some(p) = req.get("parallel_tool_calls") {
                out["parallel_tool_calls"] = p.clone();
            }
        }
    }
    if let Some(fmt) = req.get("text").and_then(|t| t.get("format")) {
        if let Some(rf) = convert_text_format(fmt) {
            out["response_format"] = rf;
        }
    }
    if let Some(reasoning) = req.get("reasoning") {
        // 上游不认 reasoning 对象；这里只把 effort 带去（它接受 reasoning_effort）
        if let Some(effort) = reasoning.get("effort").and_then(|v| v.as_str()) {
            out["reasoning_effort"] = json!(effort);
        }
    }

    Ok(out)
}

fn convert_text_format(fmt: &Value) -> Option<Value> {
    match fmt.get("type").and_then(|v| v.as_str()) {
        Some("json_object") => Some(json!({"type": "json_object"})),
        Some("json_schema") => {
            let mut schema = json!({"type": "json_schema"});
            if let Some(v) = fmt.get("name") {
                schema["json_schema"] = json!({"name": v});
            }
            if let Some(v) = fmt.get("schema") {
                schema["json_schema"]["schema"] = v.clone();
            }
            Some(schema)
        }
        Some("text") | None => None,
        _ => None,
    }
}

/// Responses 的 tool 定义 → Chat Completions 的 tool 定义。
fn responses_tool_to_chat(tool: &Value) -> Option<Value> {
    // 已经是 chat 格式
    if tool.get("function").is_some() {
        return Some(tool.clone());
    }
    let ty = tool.get("type").and_then(|v| v.as_str()).unwrap_or("function");
    if ty != "function" {
        // 内置工具（web_search / file_search / computer_use ...）上游不支持
        return None;
    }
    Some(json!({
        "type": "function",
        "function": {
            "name": tool.get("name").cloned().unwrap_or(Value::Null),
            "description": tool.get("description").cloned().unwrap_or(Value::Null),
            "parameters": tool.get("parameters").cloned().unwrap_or(json!({"type": "object"})),
        }
    }))
}

/// 单个 Responses input item → chat message。
fn input_item_to_message(item: &Value) -> Result<Option<Value>, ApiError> {
    let ty = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");

    match ty {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = item.get("content").cloned().unwrap_or(Value::Null);
            let content = normalize_content(&content);
            Ok(Some(json!({"role": role, "content": content})))
        }
        "function_call" => Ok(Some(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": item.get("call_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": item.get("arguments").cloned().unwrap_or(json!("{}")),
                }
            }]
        }))),
        "function_call_output" => {
            let output = item.get("output").cloned().unwrap_or(json!(""));
            let text = match output {
                Value::String(s) => s,
                other => other.to_string(),
            };
            Ok(Some(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": text,
            })))
        }
        // reasoning item：上游没有对应表示，丢弃（它由 assistant 历史隐式携带）
        "reasoning" => Ok(None),
        _ => Ok(None),
    }
}

/// Responses 的 content 数组 → chat content（字符串或多模态数组）。
fn normalize_content(content: &Value) -> Value {
    match content {
        Value::String(s) => json!(s),
        Value::Array(parts) => {
            let mut has_image = false;
            let mut converted = Vec::new();
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        converted.push(json!({
                            "type": "text",
                            "text": part.get("text").cloned().unwrap_or(json!("")),
                        }));
                    }
                    Some("input_image") => {
                        has_image = true;
                        let url = part
                            .get("image_url")
                            .cloned()
                            .or_else(|| part.get("url").cloned())
                            .unwrap_or(json!(""));
                        converted.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    _ => {}
                }
            }
            if has_image {
                Value::Array(converted)
            } else {
                // 纯文本时压平，兼容只认字符串的上游
                let text = converted
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                json!(text)
            }
        }
        _ => json!(""),
    }
}

/// Chat completion → Responses 的 `response` 对象。
pub fn from_chat_completion(completion: &Value, original: &Value) -> Value {
    let id = format!("resp_{}", crate::chat::uuid_ish());
    let model = completion
        .get("model")
        .cloned()
        .or_else(|| original.get("model").cloned())
        .unwrap_or(Value::Null);

    let mut output: Vec<Value> = Vec::new();
    let mut output_text = String::new();

    if let Some(choice) = completion
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    {
        let message = choice.get("message").cloned().unwrap_or(json!({}));

        let reasoning_text = message
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !reasoning_text.is_empty() {
            output.push(json!({
                "type": "reasoning",
                "id": format!("rs_{}", crate::chat::uuid_ish()),
                "summary": [],
                "content": [{"type": "reasoning_text", "text": reasoning_text}],
            }));
        }

        if let Some(content) = message.get("content").and_then(|v| v.as_str()) {
            if !content.is_empty() {
                output_text = content.to_string();
            }
        }

        // 上游在 max_output_tokens 偏小时可能只吐 reasoning、content 为空，
        // 甚至干脆没有 content 字段。此时如果 output 为空，客户端会看到
        // "completed 但什么都没有"。用推理内容兜底，避免静默空响应。
        if output_text.is_empty() && !reasoning_text.is_empty() {
            output_text = reasoning_text.to_string();
        }

        if !output_text.is_empty() {
            output.push(json!({
                "type": "message",
                "id": format!("msg_{}", crate::chat::uuid_ish()),
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": output_text,
                    "annotations": [],
                }],
            }));
        }

        if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                let func = call.get("function").cloned().unwrap_or(json!({}));
                output.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{}", crate::chat::uuid_ish()),
                    "call_id": call.get("id").cloned().unwrap_or(Value::Null),
                    "name": func.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": func.get("arguments").cloned().unwrap_or(json!("{}")),
                    "status": "completed",
                }));
            }
        }
    }

    let usage = completion.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let reasoning_tokens = usage
        .and_then(|u| u.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    json!({
        "id": id,
        "object": "response",
        "created_at": completion.get("created").cloned().unwrap_or(json!(chrono::Utc::now().timestamp())),
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": output_text,
        "parallel_tool_calls": true,
        "tool_choice": original.get("tool_choice").cloned().unwrap_or(json!("auto")),
        "tools": original.get("tools").cloned().unwrap_or(json!([])),
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": output_tokens,
            "output_tokens_details": {"reasoning_tokens": reasoning_tokens},
            "total_tokens": input_tokens + output_tokens,
        },
        "error": Value::Null,
        "incomplete_details": Value::Null,
        "instructions": original.get("instructions").cloned().unwrap_or(Value::Null),
        "metadata": original.get("metadata").cloned().unwrap_or(json!({})),
    })
}

/// 把上游 chunk 流转成 Responses 语义化 SSE 事件。
pub fn stream_body(stream: EventStream, model: Option<&str>) -> Response {
    let model = model.unwrap_or("").to_string();
    let response_id = format!("resp_{}", crate::chat::uuid_ish());

    let body = async_stream::stream! {
        use futures_util::StreamExt;

        let mut inner = stream;
        let mut sequence: u64 = 0;
        let mut item_index: i64 = -1;
        let mut output_item_open = false;
        let mut text_open = false;
        let mut reasoning_open = false;
        let mut tool_items: std::collections::BTreeMap<i64, (String, String, String)> =
            std::collections::BTreeMap::new();

        // response.created
        sequence += 1;
        yield sse_event("response.created", json!({
            "type": "response.created",
            "sequence_number": sequence,
            "response": {
                "id": response_id,
                "object": "response",
                "created_at": chrono::Utc::now().timestamp(),
                "status": "in_progress",
                "model": model,
                "output": [],
            }
        }));

        sequence += 1;
        yield sse_event("response.in_progress", json!({
            "type": "response.in_progress",
            "sequence_number": sequence,
            "response": {
                "id": response_id,
                "object": "response",
                "status": "in_progress",
                "model": model,
            }
        }));

        while let Some(item) = inner.next().await {
            match item {
                Ok(StreamEvent::Chunk(Value::Null)) => {
                    yield ": keepalive\n\n".to_string();
                }
                Ok(StreamEvent::Chunk(chunk)) => {
                    let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
                        continue;
                    };
                    for choice in choices {
                        let Some(delta) = choice.get("delta") else { continue };

                        // 推理内容 → reasoning item
                        if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                            if !r.is_empty() {
                                if !reasoning_open {
                                    item_index += 1;
                                    reasoning_open = true;
                                    sequence += 1;
                                    yield sse_event("response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "sequence_number": sequence,
                                        "output_index": item_index,
                                        "item": {
                                            "type": "reasoning",
                                            "id": format!("rs_{}", crate::chat::uuid_ish()),
                                            "summary": [],
                                        }
                                    }));
                                }
                                sequence += 1;
                                yield sse_event("response.reasoning_text.delta", json!({
                                    "type": "response.reasoning_text.delta",
                                    "sequence_number": sequence,
                                    "item_id": format!("rs_{item_index}"),
                                    "output_index": item_index,
                                    "content_index": 0,
                                    "delta": r,
                                }));
                            }
                        }

                        // 正文 → message item
                        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                if !output_item_open {
                                    item_index += 1;
                                    output_item_open = true;
                                    sequence += 1;
                                    yield sse_event("response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "sequence_number": sequence,
                                        "output_index": item_index,
                                        "item": {
                                            "type": "message",
                                            "id": format!("msg_{}", crate::chat::uuid_ish()),
                                            "status": "in_progress",
                                            "role": "assistant",
                                            "content": [],
                                        }
                                    }));
                                }
                                if !text_open {
                                    text_open = true;
                                    sequence += 1;
                                    yield sse_event("response.content_part.added", json!({
                                        "type": "response.content_part.added",
                                        "sequence_number": sequence,
                                        "item_id": format!("msg_{item_index}"),
                                        "output_index": item_index,
                                        "content_index": 0,
                                        "part": {"type": "output_text", "text": "", "annotations": []},
                                    }));
                                }
                                sequence += 1;
                                yield sse_event("response.output_text.delta", json!({
                                    "type": "response.output_text.delta",
                                    "sequence_number": sequence,
                                    "item_id": format!("msg_{item_index}"),
                                    "output_index": item_index,
                                    "content_index": 0,
                                    "delta": text,
                                }));
                            }
                        }

                        // 工具调用
                        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                            for call in calls {
                                let idx = call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                let entry = tool_items.entry(idx).or_insert_with(|| {
                                    (String::new(), String::new(), String::new())
                                });
                                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                                    if !id.is_empty() {
                                        entry.0 = id.to_string();
                                    }
                                }
                                if let Some(func) = call.get("function") {
                                    if let Some(n) = func.get("name").and_then(|v| v.as_str()) {
                                        entry.1.push_str(n);
                                    }
                                    if let Some(a) = func.get("arguments").and_then(|v| v.as_str()) {
                                        entry.2.push_str(a);
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(StreamEvent::Done) => break,
                Err(e) => {
                    sequence += 1;
                    yield sse_event("response.failed", json!({
                        "type": "response.failed",
                        "sequence_number": sequence,
                        "response": {
                            "id": response_id,
                            "object": "response",
                            "status": "failed",
                            "model": model,
                            "error": {"code": e.code, "message": e.message},
                        }
                    }));
                    return;
                }
            }
        }

        // 收尾：正文 part / item done
        if text_open {
            sequence += 1;
            yield sse_event("response.output_text.done", json!({
                "type": "response.output_text.done",
                "sequence_number": sequence,
                "item_id": format!("msg_{item_index}"),
                "output_index": item_index,
                "content_index": 0,
                "text": "",
            }));
            sequence += 1;
            yield sse_event("response.content_part.done", json!({
                "type": "response.content_part.done",
                "sequence_number": sequence,
                "item_id": format!("msg_{item_index}"),
                "output_index": item_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            }));
        }
        if output_item_open || reasoning_open {
            sequence += 1;
            yield sse_event("response.output_item.done", json!({
                "type": "response.output_item.done",
                "sequence_number": sequence,
                "output_index": item_index,
                "item": {"type": "message", "status": "completed", "role": "assistant"},
            }));
        }

        // 工具调用 item
        for (idx, (call_id, name, args)) in &tool_items {
            item_index += 1;
            sequence += 1;
            yield sse_event("response.output_item.added", json!({
                "type": "response.output_item.added",
                "sequence_number": sequence,
                "output_index": item_index,
                "item": {
                    "type": "function_call",
                    "id": format!("fc_{}", crate::chat::uuid_ish()),
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                }
            }));
            sequence += 1;
            yield sse_event("response.function_call_arguments.delta", json!({
                "type": "response.function_call_arguments.delta",
                "sequence_number": sequence,
                "item_id": format!("fc_{idx}"),
                "output_index": item_index,
                "delta": args,
            }));
        }

        sequence += 1;
        yield sse_event("response.completed", json!({
            "type": "response.completed",
            "sequence_number": sequence,
            "response": {
                "id": response_id,
                "object": "response",
                "status": "completed",
                "model": model,
            }
        }));
    };

    let _ = body; // 类型推断辅助

    let body = async_stream::stream! {
        use futures_util::StreamExt;
        let mut s = Box::pin(body);
        while let Some(item) = s.next().await {
            yield Ok::<bytes::Bytes, std::io::Error>(bytes::Bytes::from(item));
        }
    };

    crate::chat::sse_response(axum::body::Body::from_stream(body))
}

fn sse_event(event: &str, payload: Value) -> String {
    format!("event: {event}\ndata: {}\n\n", payload)
}

