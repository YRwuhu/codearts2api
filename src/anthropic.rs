//! Anthropic **Messages API** (`POST /v1/messages`) ←→ Chat Completions 互转。
//!
//! 供 Claude Code / Anthropic SDK 直接使用本站点。要点：
//!
//! - `system` 是顶层字段（字符串或 block 数组），不是 messages 里的一条
//! - `content` 是 block 数组：`text` / `image` / `tool_use` / `tool_result`
//! - 工具结果放在 **user** 消息的 `tool_result` block 里，而 chat 需要
//!   独立的 `role: "tool"` 消息
//! - `tool_use` 的入参是 JSON 对象，chat 需要 JSON **字符串**
//! - 流式事件固定有序：`message_start` → `content_block_start` →
//!   `content_block_delta`* → `content_block_stop` → `message_delta` →
//!   `message_stop`
//! - `cache_control` 上游不支持，直接丢弃

use axum::response::Response;
use serde_json::{json, Value};

use crate::chat::{ApiError, EventStream, StreamEvent};

/// Anthropic 请求 → OpenAI chat 请求。
pub fn to_chat_request(req: &Value) -> Result<Value, ApiError> {
    let mut messages: Vec<Value> = Vec::new();

    // system
    if let Some(system) = req.get("system") {
        let text = blocks_to_text(system);
        if !text.trim().is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    let Some(input) = req.get("messages").and_then(|m| m.as_array()) else {
        return Err(ApiError::bad_request("缺少 messages 字段"));
    };

    for msg in input {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        let content = msg.get("content").cloned().unwrap_or(json!(""));

        // 字符串内容直接透传
        if let Value::String(text) = &content {
            messages.push(json!({"role": role, "content": text}));
            continue;
        }

        let Some(blocks) = content.as_array() else {
            messages.push(json!({"role": role, "content": ""}));
            continue;
        };

        // 分离：文本/图片 → 一条消息；tool_use → assistant.tool_calls；
        //        tool_result → 独立的 role:tool 消息
        let mut parts: Vec<Value> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();

        for block in blocks {
            match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                }
                Some("image") => {
                    if let Some(source) = block.get("source") {
                        if let Some(url) = anthropic_image_url(source) {
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(json!({
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": {
                            "name": block.get("name").cloned().unwrap_or(Value::Null),
                            // Anthropic 给的是对象，chat 要字符串
                            "arguments": block.get("input")
                                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
                                .unwrap_or_else(|| "{}".to_string()),
                        }
                    }));
                }
                Some("tool_result") => {
                    let out = block.get("content").cloned().unwrap_or(json!(""));
                    let text = blocks_to_text(&out);
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                        "content": text,
                    }));
                }
                // thinking / redacted_thinking / cache_control 等：上游没有对应概念
                _ => {}
            }
        }

        if !tool_results.is_empty() {
            messages.extend(tool_results);
        }

        if !tool_calls.is_empty() {
            let content_value = if parts.is_empty() {
                Value::Null
            } else if parts.iter().all(|p| p.get("type") == Some(&json!("text"))) {
                json!(parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"))
            } else {
                Value::Array(parts)
            };
            messages.push(json!({
                "role": "assistant",
                "content": content_value,
                "tool_calls": tool_calls,
            }));
        } else if !parts.is_empty() {
            let content_value = if parts.iter().all(|p| p.get("type") == Some(&json!("text"))) {
                json!(parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"))
            } else {
                Value::Array(parts)
            };
            messages.push(json!({"role": role, "content": content_value}));
        }
    }

    if messages.is_empty() {
        return Err(ApiError::bad_request("messages 中没有任何可用内容"));
    }

    let mut out = json!({
        "model": req.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
    });

    if let Some(v) = req.get("max_tokens") {
        out["max_tokens"] = v.clone();
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = req.get(key) {
            out[key] = v.clone();
        }
    }
    // stop_sequences → stop
    if let Some(stop) = req.get("stop_sequences") {
        if stop.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            out["stop"] = stop.clone();
        }
    }
    // tools
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let converted: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                // Anthropic 自定义工具没有 type 字段
                let name = t.get("name")?.as_str()?;
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t.get("input_schema").cloned().unwrap_or(json!({"type": "object"})),
                    }
                }))
            })
            .collect();
        if !converted.is_empty() {
            out["tools"] = Value::Array(converted);
            if let Some(tc) = req.get("tool_choice") {
                out["tool_choice"] = convert_tool_choice(tc);
            }
        }
    }

    Ok(out)
}

fn convert_tool_choice(tc: &Value) -> Value {
    match tc.get("type").and_then(|v| v.as_str()) {
        Some("auto") => json!("auto"),
        Some("any") => json!("required"),
        Some("none") => json!("none"),
        Some("tool") => json!({
            "type": "function",
            "function": {"name": tc.get("name").cloned().unwrap_or(Value::Null)}
        }),
        _ => json!("auto"),
    }
}

fn anthropic_image_url(source: &Value) -> Option<String> {
    match source.get("type").and_then(|v| v.as_str()) {
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png");
            let data = source.get("data").and_then(|v| v.as_str())?;
            Some(format!("data:{media};base64,{data}"))
        }
        Some("url") => source
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

/// block 数组（或字符串）→ 纯文本。
pub fn blocks_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(|v| v.as_str()) {
                Some("text") => b.get("text").and_then(|v| v.as_str()),
                // tool_result 的内容本身也可能是 block 数组，递归展开
                Some("tool_result") => None,
                _ => b.get("text").and_then(|v| v.as_str()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Chat completion → Anthropic `message` 对象。
pub fn from_chat_completion(completion: &Value, model: &str, original: &Value) -> Value {
    let mut content: Vec<Value> = Vec::new();

    if let Some(choice) = completion
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    {
        let message = choice.get("message").cloned().unwrap_or(json!({}));

        let reasoning = message
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !reasoning.is_empty() {
            // 用 thinking block 表达推理内容
            content.push(json!({"type": "thinking", "thinking": reasoning}));
        }

        let text = message.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
        // 注意：这里**不要**用 reasoning 兜底成 text。
        // 推理内容已经放进 thinking block 了，再复制一份到 text 会让客户端
        // 看到同一段话出现两次（模型推理 + 正文一模一样），这正是
        // "一直重复一段话"的来源之一。真正空响应的情况由调用方按
        // finish_reason 判断，不该靠复制内容掩盖。

        if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                let func = call.get("function").cloned().unwrap_or(json!({}));
                let args = func
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                // Anthropic 要对象，chat 给的是字符串
                let input: Value =
                    serde_json::from_str(args).unwrap_or_else(|_| json!({"_raw": args}));
                content.push(json!({
                    "type": "tool_use",
                    "id": call.get("id").cloned().unwrap_or(Value::Null),
                    "name": func.get("name").cloned().unwrap_or(Value::Null),
                    "input": input,
                }));
            }
        }
    }

    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
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

    let stop_reason = completion
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(|v| v.as_str())
        .map(|r| match r {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            "content_filter" => "refusal",
            _ => "end_turn",
        })
        .unwrap_or("end_turn");

    json!({
        "id": format!("msg_{}", crate::chat::uuid_ish()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        },
        // 非标准字段：方便客户端调试，Anthropic 官方也会忽略
        "_request_model": original.get("model").cloned().unwrap_or(Value::Null),
    })
}

/// 上游 chunk 流 → Anthropic SSE 事件。
pub fn sse_body(stream: EventStream, model: String, _original: Option<Value>) -> Response {
    let message_id = format!("msg_{}", crate::chat::uuid_ish());

    let body = async_stream::stream! {
        use futures_util::StreamExt;

        let mut inner = stream;

        // Anthropic 要求 block index 从 0 开始且严格递增，每个 start 都必须
        // 配对一个 stop。旧实现从 1 开始，且 thinking / text 共用计数器，
        // 导致 thinking block 永远不关闭 —— 客户端会一直等这个 block 结束，
        // 表现就是内容重复刷、工具调用解析错乱。
        let mut next_index: i64 = 0;
        // 当前打开的 block：(index, kind)
        let mut open_block: Option<(i64, &'static str)> = None;

        let mut input_tokens: i64 = 0;
        let mut output_tokens: i64 = 0;
        let mut saw_text = false;
        let mut saw_thinking = false;

        // 工具调用按上游 index 聚合：id / name / arguments
        let mut tool_calls: std::collections::BTreeMap<i64, (String, String, String)> =
            std::collections::BTreeMap::new();

        yield anthropic_event("message_start", json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }
        }));

        yield anthropic_event("ping", json!({"type": "ping"}));

        // 切换 block：先关掉当前打开的，再开一个新的。
        macro_rules! switch_block {
            ($kind:expr, $start_payload:expr) => {{
                if let Some((idx, _)) = open_block.take() {
                    yield anthropic_event("content_block_stop", json!({
                        "type": "content_block_stop",
                        "index": idx,
                    }));
                }
                let idx = next_index;
                next_index += 1;
                open_block = Some((idx, $kind));
                let mut payload = $start_payload;
                payload["index"] = json!(idx);
                yield anthropic_event("content_block_start", payload);
            }};
        }

        while let Some(item) = inner.next().await {
            match item {
                Ok(StreamEvent::Chunk(Value::Null)) => {
                    yield anthropic_event("ping", json!({"type": "ping"}));
                }
                Ok(StreamEvent::Chunk(chunk)) => {
                    if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                        if let Some(v) = u.get("prompt_tokens").and_then(|v| v.as_i64()) {
                            input_tokens = v;
                        }
                        if let Some(v) = u.get("completion_tokens").and_then(|v| v.as_i64()) {
                            output_tokens = v;
                        }
                    }

                    let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) else {
                        continue;
                    };
                    for choice in choices {
                        let Some(delta) = choice.get("delta") else { continue };

                        // ── thinking ──
                        if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                            if !r.is_empty() {
                                saw_thinking = true;
                                if !matches!(open_block, Some((_, "thinking"))) {
                                    switch_block!("thinking", json!({
                                        "type": "content_block_start",
                                        "index": 0,
                                        "content_block": {"type": "thinking", "thinking": ""},
                                    }));
                                }
                                let idx = open_block.map(|(i, _)| i).unwrap_or(0);
                                yield anthropic_event("content_block_delta", json!({
                                    "type": "content_block_delta",
                                    "index": idx,
                                    "delta": {"type": "thinking_delta", "thinking": r},
                                }));
                            }
                        }

                        // ── text ──
                        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                saw_text = true;
                                if !matches!(open_block, Some((_, "text"))) {
                                    switch_block!("text", json!({
                                        "type": "content_block_start",
                                        "index": 0,
                                        "content_block": {"type": "text", "text": ""},
                                    }));
                                }
                                let idx = open_block.map(|(i, _)| i).unwrap_or(0);
                                yield anthropic_event("content_block_delta", json!({
                                    "type": "content_block_delta",
                                    "index": idx,
                                    "delta": {"type": "text_delta", "text": text},
                                }));
                            }
                        }

                        // ── tool calls：先聚合，流末尾统一发 ──
                        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                            for call in calls {
                                let idx = call.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                                let entry = tool_calls.entry(idx).or_insert_with(|| {
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
                    // 先关掉打开的 block 再报错，避免客户端卡在半截状态
                    if let Some((idx, _)) = open_block.take() {
                        yield anthropic_event("content_block_stop", json!({
                            "type": "content_block_stop",
                            "index": idx,
                        }));
                    }
                    yield anthropic_event("error", json!({
                        "type": "error",
                        "error": {"type": "api_error", "message": e.message},
                    }));
                    return;
                }
            }
        }

        // 关闭正文/推理 block
        if let Some((idx, _)) = open_block.take() {
            yield anthropic_event("content_block_stop", json!({
                "type": "content_block_stop",
                "index": idx,
            }));
        }

        // 工具调用：每个一个独立 block。input_json_delta 要传**原始 JSON 片段**，
        // 不是重新序列化的对象 —— 旧实现传了 to_string() 的完整对象，
        // 客户端按流式片段拼接时就会解析失败。
        for (_upstream_idx, (call_id, name, args)) in &tool_calls {
            let idx = next_index;
            next_index += 1;
            yield anthropic_event("content_block_start", json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": if call_id.is_empty() {
                        format!("toolu_{}", crate::chat::uuid_ish())
                    } else {
                        call_id.clone()
                    },
                    "name": name,
                    "input": {},
                },
            }));
            yield anthropic_event("content_block_delta", json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "input_json_delta", "partial_json": args},
            }));
            yield anthropic_event("content_block_stop", json!({
                "type": "content_block_stop",
                "index": idx,
            }));
        }

        // 什么都没产出时补一个空 text block，避免客户端拿到空 content 数组
        if !saw_thinking && !saw_text && tool_calls.is_empty() {
            yield anthropic_event("content_block_start", json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }));
            yield anthropic_event("content_block_stop", json!({
                "type": "content_block_stop",
                "index": 0,
            }));
        }

        let stop_reason = if !tool_calls.is_empty() { "tool_use" } else { "end_turn" };

        yield anthropic_event("message_delta", json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        }));

        yield anthropic_event("message_stop", json!({"type": "message_stop"}));
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

fn anthropic_event(event: &str, payload: Value) -> String {
    format!("event: {event}\ndata: {}\n\n", payload)
}

/// 粗略估算输入 token 数（`/v1/messages/count_tokens` 用）。
///
/// 没有 tokenizer，只能按经验值：中文约 1 字 1 token，英文约 4 字符 1 token。
/// Anthropic 客户端只用它做预算判断，精确值并不要求。
pub fn estimate_tokens(req: &Value) -> i64 {
    let mut chars = 0usize;
    if let Some(system) = req.get("system") {
        chars += blocks_to_text(system).chars().count();
    }
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let content = m.get("content").cloned().unwrap_or(json!(""));
            chars += blocks_to_text(&content).chars().count();
            // 图片按固定成本估
            if let Some(blocks) = content.as_array() {
                chars += blocks.iter().filter(|b| {
                    b.get("type").and_then(|v| v.as_str()) == Some("image")
                }).count() * 4000;
            }
        }
    }
    // 工具定义也占 token
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        chars += tools
            .iter()
            .map(|t| serde_json::to_string(t).map(|s| s.len()).unwrap_or(0))
            .sum::<usize>()
            / 4;
    }
    ((chars / 3).max(1)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_simple_message() {
        let req = json!({
            "model": "deepseek-v4.1-flash",
            "max_tokens": 100,
            "system": "你是助手",
            "messages": [{"role": "user", "content": "你好"}],
        });
        let chat = to_chat_request(&req).unwrap();
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "你是助手");
        assert_eq!(chat["messages"][1]["content"], "你好");
        assert_eq!(chat["max_tokens"], 100);
    }

    #[test]
    fn converts_tool_use_and_result() {
        let req = json!({
            "model": "m",
            "max_tokens": 10,
            "messages": [
                {"role": "user", "content": "天气?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "北京"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "晴"}
                    ]}
                ]}
            ],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]
        });
        let chat = to_chat_request(&req).unwrap();
        let msgs = chat["messages"].as_array().unwrap();
        // user / assistant(tool_calls) / tool
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        // 入参必须是字符串
        assert!(msgs[1]["tool_calls"][0]["function"]["arguments"].is_string());
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "t1");
        assert_eq!(msgs[2]["content"], "晴");
        assert_eq!(chat["tools"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn converts_completion_to_message() {
        let completion = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "好的"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });
        let msg = from_chat_completion(&completion, "m", &json!({}));
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "好的");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["usage"]["input_tokens"], 5);
    }

    #[test]
    fn maps_tool_calls_to_tool_use() {
        let completion = json!({
            "choices": [{
                "message": {"content": null, "tool_calls": [{
                    "id": "c1",
                    "function": {"name": "f", "arguments": "{\"a\":1}"}
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let msg = from_chat_completion(&completion, "m", &json!({}));
        let tool = msg["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "tool_use")
            .unwrap();
        assert_eq!(tool["input"]["a"], 1);
        assert_eq!(msg["stop_reason"], "tool_use");
    }
}
