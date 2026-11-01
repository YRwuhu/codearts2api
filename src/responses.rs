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

struct ResponseTool {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: i64,
    started: bool,
}

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
                for message in input_item_to_messages(item)? {
                    messages.push(message);
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
    let ty = tool
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("function");
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

/// 单个 Responses input item → chat messages。
///
/// 工具结果里的图片需放入紧随其后的用户消息，因为上游不支持 tool 图片。
fn input_item_to_messages(item: &Value) -> Result<Vec<Value>, ApiError> {
    let ty = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("message");

    match ty {
        "message" => {
            let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = item.get("content").cloned().unwrap_or(Value::Null);
            let content = normalize_content(&content)?;
            Ok(vec![json!({"role": role, "content": content})])
        }
        "function_call" => Ok(vec![json!({
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
        })]),
        "function_call_output" => {
            let output = item.get("output").cloned().unwrap_or(json!(""));
            let (text, images) = function_output_content(&output)?;
            let mut messages = vec![json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": text,
            })];
            if !images.is_empty() {
                messages.push(json!({"role": "user", "content": images}));
            }
            Ok(messages)
        }
        // reasoning item：上游没有对应表示，丢弃（它由 assistant 历史隐式携带）
        "reasoning" => Ok(Vec::new()),
        _ => Ok(Vec::new()),
    }
}

fn function_output_content(output: &Value) -> Result<(String, Vec<Value>), ApiError> {
    match output {
        Value::String(s) => Ok((s.clone(), Vec::new())),
        Value::Array(parts) => {
            let mut text = Vec::new();
            let mut images = Vec::new();
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        if let Some(s) = part.get("text").and_then(|v| v.as_str()) {
                            text.push(s.to_string());
                        }
                    }
                    Some("input_image") | Some("image") => {
                        images.push(responses_image_part(part)?);
                    }
                    _ => text.push(part.to_string()),
                }
            }
            Ok((text.join("\n"), images))
        }
        other => Ok((other.to_string(), Vec::new())),
    }
}

fn responses_image_part(part: &Value) -> Result<Value, ApiError> {
    let url = part
        .get("image_url")
        .or_else(|| part.get("url"))
        .filter(|v| !v.is_null());
    let Some(url) = url else {
        if part.get("file_id").is_some() {
            return Err(ApiError::bad_request(
                "暂不支持通过 file_id 发送图片，请使用 image_url 或 base64 data URL",
            ));
        }
        return Err(ApiError::bad_request("input_image 缺少 image_url 或 url"));
    };
    let detail = part.get("detail").or_else(|| url.get("detail")).cloned();
    let url = match url {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Object(obj) => obj
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ApiError::bad_request("input_image 的 image_url 缺少有效 URL"))?,
        _ => {
            return Err(ApiError::bad_request(
                "input_image 的图片 URL 必须是非空字符串",
            ))
        }
    };
    let mut image_url = json!({"url": url});
    if let Some(detail) = detail {
        image_url["detail"] = detail;
    }
    Ok(json!({"type": "image_url", "image_url": image_url}))
}

/// Responses 的 content 数组 → chat content（字符串或多模态数组）。
fn normalize_content(content: &Value) -> Result<Value, ApiError> {
    match content {
        Value::String(s) => Ok(json!(s)),
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
                    Some("input_image") | Some("image") => {
                        has_image = true;
                        converted.push(responses_image_part(part)?);
                    }
                    _ => {}
                }
            }
            if has_image {
                Ok(Value::Array(converted))
            } else {
                // 纯文本时压平，兼容只认字符串的上游
                let text = converted
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(json!(text))
            }
        }
        _ => Ok(json!("")),
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
        let mut message_text = String::new();
        let mut reasoning_open = false;
        let mut reasoning_text = String::new();
        let mut reasoning_id = String::new();
        let mut reasoning_index = 0;
        let mut message_id = String::new();
        let mut message_index = 0;
        let mut tool_items: std::collections::BTreeMap<i64, ResponseTool> =
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
                                    reasoning_index = item_index + 1;
                                    item_index = reasoning_index;
                                    reasoning_id = format!("rs_{}", crate::chat::uuid_ish());
                                    reasoning_open = true;
                                    sequence += 1;
                                    yield sse_event("response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "sequence_number": sequence,
                                        "output_index": reasoning_index,
                                        "item": {
                                            "type": "reasoning",
                                            "id": reasoning_id,
                                            "summary": [],
                                        }
                                    }));
                                    reasoning_text.push_str(r);
                                }
                                sequence += 1;
                                yield sse_event("response.reasoning_text.delta", json!({
                                    "type": "response.reasoning_text.delta",
                                    "sequence_number": sequence,
                                    "item_id": reasoning_id,
                                    "output_index": reasoning_index,
                                    "content_index": 0,
                                    "delta": r,
                                }));
                            }
                        }

                        // 正文 → message item
                        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                if !output_item_open {
                                    message_index = item_index + 1;
                                    item_index = message_index;
                                    message_id = format!("msg_{}", crate::chat::uuid_ish());
                                    output_item_open = true;
                                    sequence += 1;
                                    yield sse_event("response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "sequence_number": sequence,
                                        "output_index": message_index,
                                        "item": {
                                            "type": "message",
                                            "id": message_id,
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
                                        "item_id": message_id,
                                        "output_index": message_index,
                                        "content_index": 0,
                                        "part": {"type": "output_text", "text": "", "annotations": []},
                                    }));
                                    message_text.push_str(text);
                                }
                                sequence += 1;
                                yield sse_event("response.output_text.delta", json!({
                                    "type": "response.output_text.delta",
                                    "sequence_number": sequence,
                                    "item_id": message_id,
                                    "output_index": message_index,
                                    "content_index": 0,
                                    "delta": text,
                                }));
                            }
                        }

                        // 工具 item 与参数 delta 实时输出，item ID 全程一致。
                        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                            for call in calls {
                                let idx = call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                let func = call.get("function");
                                let name = func.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or("");
                                let args = func.and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or("");
                                let is_new = {
                                    let entry = tool_items.entry(idx).or_insert_with(|| {
                                        item_index += 1;
                                        ResponseTool {
                                            item_id: format!("fc_{}", crate::chat::uuid_ish()),
                                            call_id: String::new(),
                                            name: String::new(),
                                            arguments: String::new(),
                                            output_index: item_index,
                                            started: false,
                                        }
                                    });
                                    if let Some(id) = call.get("id").and_then(|v| v.as_str()).filter(|id| !id.is_empty()) {
                                        if entry.call_id.is_empty() {
                                            entry.call_id = id.to_string();
                                        }
                                    }
                                    entry.name.push_str(name);
                                    entry.arguments.push_str(args);
                                    let is_new = !entry.started;
                                    entry.started = true;
                                    is_new
                                };
                                let (item_id, call_id, tool_name, output_index) = {
                                    let entry = tool_items.get(&idx).expect("tool exists");
                                    (entry.item_id.clone(), entry.call_id.clone(), entry.name.clone(), entry.output_index)
                                };
                                if !tool_items.get(&idx).map(|t| !t.arguments.is_empty() || !t.name.is_empty()).unwrap_or(false) {
                                    continue;
                                }
                                if is_new {
                                    sequence += 1;
                                    yield sse_event("response.output_item.added", json!({
                                        "type": "response.output_item.added",
                                        "sequence_number": sequence,
                                        "output_index": output_index,
                                        "item": {
                                            "type": "function_call", "id": item_id,
                                            "call_id": call_id, "name": tool_name,
                                            "arguments": "", "status": "in_progress",
                                        }
                                    }));
                                }
                                if !args.is_empty() {
                                    sequence += 1;
                                    yield sse_event("response.function_call_arguments.delta", json!({
                                        "type": "response.function_call_arguments.delta",
                                        "sequence_number": sequence,
                                        "item_id": item_id,
                                        "output_index": output_index,
                                        "delta": args,
                                    }));
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

        // 收尾：每个 item 使用创建时的 ID 与 output_index。
        if reasoning_open {
            sequence += 1;
            yield sse_event("response.reasoning_text.done", json!({
                "type": "response.reasoning_text.done", "sequence_number": sequence,
                "item_id": reasoning_id, "output_index": reasoning_index,
                "content_index": 0, "text": reasoning_text,
            }));
            sequence += 1;
            yield sse_event("response.output_item.done", json!({
                "type": "response.output_item.done", "sequence_number": sequence,
                "output_index": reasoning_index,
                "item": {"type": "reasoning", "id": reasoning_id, "summary": []},
            }));
        }
        if text_open {
            sequence += 1;
            yield sse_event("response.output_text.done", json!({
                "type": "response.output_text.done", "sequence_number": sequence,
                "item_id": message_id, "output_index": message_index,
                "content_index": 0, "text": message_text,
            }));
            sequence += 1;
            yield sse_event("response.content_part.done", json!({
                "type": "response.content_part.done", "sequence_number": sequence,
                "item_id": message_id, "output_index": message_index, "content_index": 0,
                "part": {"type": "output_text", "text": message_text, "annotations": []},
            }));
        }
        if output_item_open {
            sequence += 1;
            yield sse_event("response.output_item.done", json!({
                "type": "response.output_item.done", "sequence_number": sequence,
                "output_index": message_index,
                "item": {"type": "message", "id": message_id, "status": "completed", "role": "assistant"},
            }));
        }

        for tool in tool_items.values() {
            sequence += 1;
            yield sse_event("response.function_call_arguments.done", json!({
                "type": "response.function_call_arguments.done", "sequence_number": sequence,
                "item_id": tool.item_id, "output_index": tool.output_index,
                "arguments": tool.arguments,
            }));
            sequence += 1;
            yield sse_event("response.output_item.done", json!({
                "type": "response.output_item.done", "sequence_number": sequence,
                "output_index": tool.output_index,
                "item": {
                    "type": "function_call", "id": tool.item_id,
                    "call_id": tool.call_id, "name": tool.name,
                    "arguments": tool.arguments, "status": "completed",
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn converts_input_image_and_preserves_detail() {
        let req = json!({
            "model": "m",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "图片里有什么？"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAA", "detail": "high"}
            ]}]
        });
        let chat = to_chat_request(&req).unwrap();
        assert_eq!(
            chat["messages"][0]["content"],
            json!([
                {"type": "text", "text": "图片里有什么？"},
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64,AAA", "detail": "high"
                }}
            ])
        );
    }

    #[test]
    fn preserves_images_in_function_call_output() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "screenshot", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": [
                    {"type": "input_text", "text": "已截图"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAA"}
                ]}
            ]
        });
        let chat = to_chat_request(&req).unwrap();
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["content"], "已截图");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAA"
        );
        assert!(!messages[1].to_string().contains("base64"));
    }

    #[tokio::test]
    async fn tool_arguments_stream_before_done_with_stable_ids() {
        let chunk = |arguments: &str| {
            Ok(StreamEvent::Chunk(
                json!({"choices":[{"delta":{"tool_calls":[{
                    "index":0,"id":"call-1","function":{"name":"read_file","arguments":arguments}
                }]}}]}),
            ))
        };
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let stream: EventStream = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        tx.send(chunk("{\"path\":")).await.unwrap();
        let response = stream_body(stream, Some("m"));
        let mut body = response.into_body().into_data_stream();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let frame = body.next().await.expect("stream ended").unwrap();
                let text = String::from_utf8(frame.to_vec()).unwrap();
                if text.contains("response.function_call_arguments.delta") {
                    return text;
                }
            }
        })
        .await
        .expect("tool delta must arrive before Done");
        tx.send(Ok(StreamEvent::Done)).await.unwrap();
        drop(tx);
        assert!(frame.contains("\"delta\":\"{\\\"path\\\":\""));
    }

    #[tokio::test]
    async fn tool_item_id_is_consistent_across_events() {
        let stream: EventStream = Box::pin(futures_util::stream::iter(vec![
            Ok(StreamEvent::Chunk(
                json!({"choices":[{"delta":{"tool_calls":[{
                    "index":0,"id":"call-1","function":{"name":"f","arguments":"{}"}
                }]}}]}),
            )),
            Ok(StreamEvent::Done),
        ]));
        let body = stream_body(stream, Some("m")).into_body();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let events: Vec<Value> = text
            .split("\n\n")
            .filter_map(|block| block.lines().find_map(|line| line.strip_prefix("data: ")))
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let added = events
            .iter()
            .find(|e| e["type"] == "response.output_item.added")
            .unwrap();
        let delta = events
            .iter()
            .find(|e| e["type"] == "response.function_call_arguments.delta")
            .unwrap();
        let done = events
            .iter()
            .find(|e| e["type"] == "response.function_call_arguments.done")
            .unwrap();
        assert_eq!(added["item"]["id"], delta["item_id"]);
        assert_eq!(added["item"]["id"], done["item_id"]);
        assert_eq!(added["output_index"], delta["output_index"]);
        assert_eq!(done["arguments"], "{}");
    }

    #[test]
    fn rejects_missing_image_url_instead_of_sending_empty_url() {
        let req = json!({
            "model": "m",
            "input": [{"role": "user", "content": [{"type": "input_image", "file_id": "file-1"}]}]
        });
        let error = to_chat_request(&req).unwrap_err();
        assert!(error.message.contains("file_id"));
    }
}
