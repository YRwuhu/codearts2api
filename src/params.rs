//! 请求参数归一化与上游错误映射。
//!
//! 这两块是 Python 版踩过坑之后补上的，Rust 版必须保持同样语义：
//! 1. 各客户端会用不同字段名表达输出长度（max_tokens / max_completion_tokens /
//!    max_output_tokens），上游只认 max_tokens 且超限时返回 HTTP 200 + 业务错误；
//! 2. 上游业务错误可能出现在 200 的 JSON 或 SSE 流里，必须翻译成可识别的形态。

use std::collections::{BTreeMap, VecDeque};

use serde_json::{json, Map, Value};

/// 上游各模型真正接受的 max_tokens 硬上限。
///
/// 这个值**不是**取自 `model_parameters.max_tokens`（那里恒为 65536/65535），
/// 而是上游参数校验器报出来的真实边界：超出时它会回
/// `RANGE_VALIDATOR: max_tokens -> max_tokens should be between 1 and 131072`。
/// 实测 10 个模型（deepseek-v4.1 / glm-5.3 / kimi-k3 / openpangu-2.0 ...）全部
/// 接受 131072、拒绝 131073，因此统一按这个上限裁剪。
///
/// 历史 bug：旧版直接信 `model_parameters.max_tokens`（65536），把客户端合法的
/// 128k 请求硬砍到 64k，于是"官方支持 384k 输出"和"反代只给 60k"的观感就出现了。
/// 注意 131072 是**输出**上限；模型的总上下文另见 `context_window`（1M）。
pub const UPSTREAM_MAX_OUTPUT_TOKENS: i64 = 131_072;

/// 把 max_* 系列字段统一成 max_tokens，并按模型上限裁剪。
///
/// `cap` 是调用方拿到的模型上限；它只会在**低于**上游硬上限时生效
/// （例如某个模型确实比别的模型小）。高于硬上限一律收敛到
/// [`UPSTREAM_MAX_OUTPUT_TOKENS`]，否则上游会回 400。
///
/// 返回 (是否发生裁剪, 原始值, 裁剪后值)。
pub fn normalize_params(body: &mut Map<String, Value>, cap: i64) -> Option<(i64, i64)> {
    let mut original: Option<i64> = None;
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if let Some(v) = body.get(key).and_then(|v| v.as_i64()) {
            if v > 0 {
                original = Some(original.map_or(v, |cur: i64| cur.max(v)));
            }
        }
    }

    let effective_cap = cap.clamp(1, UPSTREAM_MAX_OUTPUT_TOKENS);

    let mut capped_now = None;
    if let Some(orig) = original {
        let capped = orig.min(effective_cap);
        // 上游只认 max_tokens；其余两个字段会被当成未知参数拒绝。
        body.remove("max_completion_tokens");
        body.remove("max_output_tokens");
        body.insert("max_tokens".to_string(), Value::from(capped));
        if capped != orig {
            capped_now = Some((orig, capped));
        }
    }

    // reasoning 是 Responses API 字段，Chat Completions 上游不认。
    body.remove("reasoning");

    normalize_roles(body);
    repair_tool_pairs(body);

    capped_now
}

/// 修复 `tool_calls` / `tool` 消息配对，避免上游 400。
///
/// 上游对工具链的要求是：assistant 的每条 `tool_calls` 必须被**紧随其后、
/// 连续排列**的 `role: "tool"` 消息逐条应答。任何消息插进这段连续区间都会
/// 让后半段全部变成孤儿，报成：
///
/// ```text
/// Invalid request: an assistant message with 'tool_calls' must be followed by
/// tool messages responding to each 'tool_call_id'. The following tool_call_ids
/// did not have response messages: Read:9, Read:10, Read:11, Bash:12
/// ```
///
/// 实测来源是 `anthropic::to_chat_request`：某个 `tool_result` 带截图时，它会
/// 在工具链中间补一条 `role: "user"` 的图片消息（上游不接受 tool 带图），把
/// 后面几个 tool 结果全隔断了。适配层已改为把图片排到最后，这里再做一次
/// 兜底归一化——直通 `/v1/chat/completions` 的客户端也可能发出同样的形状。
///
/// 处理方式：
/// 1. 属于某条 assistant `tool_calls` 的 tool 消息提前到紧邻位置（保持原序）；
/// 2. 完全没有对应 `tool_calls` 的孤儿 tool 消息降级为 user 文本，否则上游会
///    因未知 `tool_call_id` 直接拒绝；
/// 3. 缺失结果的 `tool_call_id` 补一条占位 tool 消息，说明该调用被中断。
pub fn repair_tool_pairs(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };

    let len = messages.len();
    let has_tools = messages
        .iter()
        .any(|m| is_tool_message(m) || has_tool_calls(m));
    if !has_tools {
        return;
    }

    // 第一遍：按 id 把 tool 消息排成队列，再给每条 tool_calls 分配具体下标。
    // 不在这里搬运消息，避免复制带 base64 的大 payload。
    let mut available: BTreeMap<String, VecDeque<usize>> = BTreeMap::new();
    for (idx, msg) in messages.iter().enumerate() {
        if is_tool_message(msg) {
            available
                .entry(tool_call_id(msg))
                .or_default()
                .push_back(idx);
        }
    }

    // tool_calls 各 id → 对应的 tool 消息下标（缺失为 None），以及该 tool 消息
    // 已被哪条 assistant 认领。
    let mut plan: BTreeMap<usize, Vec<(String, Option<usize>)>> = BTreeMap::new();
    let mut claimed = vec![false; len];
    for (idx, msg) in messages.iter().enumerate() {
        if !has_tool_calls(msg) {
            continue;
        }
        let slots = tool_call_ids(msg)
            .into_iter()
            .map(|id| {
                let origin = available.get_mut(&id).and_then(|queue| queue.pop_front());
                if let Some(origin) = origin {
                    claimed[origin] = true;
                }
                (id, origin)
            })
            .collect();
        plan.insert(idx, slots);
    }

    // 第二遍：按原顺序重建；被认领的 tool 消息在对应 assistant 处落地，
    // 未认领的孤儿降级为 user，缺失的结果补占位。
    let mut slots: Vec<Option<Value>> = std::mem::take(messages).into_iter().map(Some).collect();
    let mut rebuilt: Vec<Value> = Vec::with_capacity(len + 8);

    for idx in 0..len {
        if slots[idx].as_ref().is_some_and(is_tool_message) {
            if claimed[idx] {
                // 已被认领：若排在 assistant 之前，先留在原位，等 assistant
                // 处理时再取走；若排在后面，则已被取走（slots 为 None），
                // 上面的 is_some_and 已经不成立。两种情况都不在这里落地。
                continue;
            }
            if let Some(orphan) = slots[idx].take() {
                rebuilt.push(relabel_as_user(orphan));
            }
            continue;
        }

        if let Some(msg) = slots[idx].take() {
            rebuilt.push(msg);
        }
        let Some(calls) = plan.remove(&idx) else {
            continue;
        };
        for (id, origin) in calls {
            match origin.and_then(|origin| slots[origin].take()) {
                Some(result) => rebuilt.push(result),
                None => rebuilt.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": TOOL_RESULT_LOST,
                })),
            }
        }
    }

    *messages = rebuilt;
}

fn is_tool_message(msg: &Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("tool")
}

/// 上游对「工具调用后没等到结果」的兜底文案。
const TOOL_RESULT_LOST: &str = "该工具调用未返回结果（对话被中断或客户端未回传），请重新发起。";

fn has_tool_calls(msg: &Value) -> bool {
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|calls| !calls.is_empty())
        .unwrap_or(false)
}

fn tool_call_ids(msg: &Value) -> Vec<String> {
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn tool_call_id(msg: &Value) -> String {
    msg.get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 孤儿 tool 消息 → 用户消息。内容原样保留（文本或多模态数组），只是换掉
/// 承载它的角色；`tool_call_id` 一并丢掉，避免上游把它当未知字段。
fn relabel_as_user(msg: Value) -> Value {
    let content = msg.get("content").cloned().unwrap_or(json!(""));
    json!({"role": "user", "content": content})
}

/// 把上游不认的 message 角色映射成它认识的。
///
/// OpenAI 的 `developer` 角色（新版 SDK 用它替代 `system`）上游不认，
/// 直接转发会得到：
///
/// ```text
/// InferHub.001001044.400 Message role cannot empty
/// ```
///
/// 实测把它改成 `system` 后请求正常。
pub fn normalize_roles(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for msg in messages {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        if obj.get("role").and_then(|r| r.as_str()) == Some("developer") {
            obj.insert("role".to_string(), Value::from("system"));
        }
    }
}

/// 单张图片解码后的上限。
///
/// 与 CodeArts 网页/IDE 的图片约束同一量级；真正的上游限制仍会生效，
/// 这里只挡住明显异常的 payload，避免白烧一次额度。
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// 原生识图的图片入参校验，返回 `true` 表示请求里确实带了图片。
///
/// 上游 `/api/v2/chat/completions` 只接受 base64 `data:` URL：远程
/// `http(s)://` 会被 `400 ModelArts.81001 unsupported image url` 拒绝。
/// 在发请求前给出可读错误，省掉一次注定失败的往返。
///
/// 顺带把「有没有图片」一起算出来，调用方不必再遍历一遍整个 body。
pub fn validate_image_input(body: &Map<String, Value>) -> Result<bool, String> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Ok(false);
    };
    let mut found = false;
    for message in messages {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if !is_image_part(part) {
                continue;
            }
            found = true;
            let url = image_url_of(part).ok_or("图片缺少 URL")?;
            check_image_url(url)?;
        }
    }
    Ok(found)
}

/// 该 content part 是否是图片块（三种协议归一化后的形状）。
fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("image_url" | "input_image" | "image")
    )
}

/// 从各家协议的图片块里取出 URL：OpenAI / Responses / Anthropic 三种形状。
fn image_url_of(part: &Value) -> Option<&str> {
    part.get("image_url")
        .and_then(|value| match value {
            Value::String(url) => Some(url.as_str()),
            other => other.get("url").and_then(Value::as_str),
        })
        .or_else(|| part.get("url").and_then(Value::as_str))
        .or_else(|| {
            part.get("source")
                .and_then(|source| source.get("url").and_then(Value::as_str))
        })
}

fn check_image_url(url: &str) -> Result<(), String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Err(
            "上游只接受 base64 data URL，不接受远程图片 URL；请先在客户端把图片内联为 \
             data:image/...;base64,..."
                .into(),
        );
    }
    let Some(data) = url.strip_prefix("data:") else {
        return Err("图片必须是 data:image/...;base64,... 格式".into());
    };
    let Some((meta, payload)) = data.split_once(',') else {
        return Err("图片 data URL 格式无效".into());
    };
    if !meta
        .split(';')
        .any(|value| value.eq_ignore_ascii_case("base64"))
    {
        return Err("图片 data URL 必须使用 base64 编码".into());
    }
    let mime = meta.split(';').next().unwrap_or("").to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/gif"
    ) {
        return Err(format!("不支持的图片类型 {mime}；支持 PNG/JPEG/WebP/GIF"));
    }
    let decoded = base64_decoded_len_capped(payload, MAX_IMAGE_BYTES);
    if decoded == 0 {
        return Err("图片内容为空".into());
    }
    if decoded > MAX_IMAGE_BYTES {
        return Err(format!(
            "单张图片超过上限 {MAX_IMAGE_BYTES} 字节（约 {} MiB）",
            MAX_IMAGE_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// 估算 base64 解码后的字节数（不计空白与补位），超过 `limit` 就提前返回。
///
/// 一张 20 MiB 的图片对应约 27 MB 的 base64 字符串；全量扫完没有意义，
/// 达到上限即可判定超限并停止。
fn base64_decoded_len_capped(payload: &str, limit: usize) -> usize {
    // 解码后字节数 ≈ 有效字符数 * 3 / 4，反推需要扫描的字符数上限。
    let max_chars = limit.saturating_mul(4) / 3 + 4;
    let mut chars = 0usize;
    for byte in payload.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        chars += 1;
        if chars > max_chars {
            return chars.saturating_mul(3) / 4;
        }
    }
    chars.saturating_mul(3) / 4
}

/// 估算 base64 解码后的字节数（不计空白与补位），用于大小校验。
#[cfg(test)]
fn base64_decoded_len(payload: &str) -> usize {
    base64_decoded_len_capped(payload, usize::MAX / 4)
}

/// 只记录多模态消息结构，绝不输出提示词、base64 或凭证。
pub fn request_shape(body: &Map<String, Value>) -> Value {
    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|messages| {
            messages
                .iter()
                .map(|message| {
                    let role = message.get("role").cloned().unwrap_or(Value::Null);
                    let content = match message.get("content") {
                        Some(Value::Array(parts)) => Value::Array(
                            parts
                                .iter()
                                .map(|part| {
                                    let mut shape = serde_json::json!({
                                        "type": part.get("type").cloned().unwrap_or(Value::Null),
                                        "fields": part.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                                    });
                                    if let Some(url) = part
                                        .get("image_url")
                                        .and_then(|v| v.get("url"))
                                        .and_then(|v| v.as_str())
                                    {
                                        let data = url.split_once(',').map(|(_, d)| d).unwrap_or("");
                                        shape["image"] = serde_json::json!({
                                            "scheme": url.split(':').next().unwrap_or(""),
                                            "mime": url.split(';').next().unwrap_or(""),
                                            "data_chars": data.len(),
                                            "url": if url.starts_with("data:") { Value::Null } else { Value::String(url.split('?').next().unwrap_or("").to_string()) },
                                        });
                                    }
                                    shape
                                })
                                .collect(),
                        ),
                        Some(Value::String(_)) => serde_json::json!("text"),
                        Some(other) => serde_json::json!({"type": other.as_str().map(|_| "text")}),
                        None => Value::Null,
                    };
                    serde_json::json!({"role": role, "content": content})
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    serde_json::json!({"messages": messages})
}

/// 上游是否把错误塞进了 HTTP 200。
pub fn is_business_error(value: &Value) -> bool {
    value.get("error_code").is_some() || value.get("error_msg").is_some()
}

/// 转成 OpenAI 错误对象。
pub fn error_payload(value: &Value) -> Value {
    serde_json::json!({
        "error": {
            "message": value.get("error_msg").and_then(|v| v.as_str())
                .unwrap_or("CodeArts upstream error"),
            "type": "upstream_error",
            "code": value.get("error_code").cloned().unwrap_or(Value::Null),
        }
    })
}

/// 结束原因归一化：CodeArts 的 "other" 语义上等于正常结束。
pub fn normalize_finish_reason(value: Option<&Value>) -> String {
    match value.and_then(|v| v.as_str()) {
        None | Some("other") => "stop".to_string(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn caps_oversized_max_tokens() {
        let mut b = body(json!({"max_tokens": 384000}));
        let capped = normalize_params(&mut b, 65536);
        assert_eq!(capped, Some((384000, 65536)));
        assert_eq!(b["max_tokens"], json!(65536));
    }

    #[test]
    fn accepts_upstream_ceiling() {
        // 上游真实上限是 131072；旧版按 65536 裁剪导致 128k 请求被腰斩。
        let mut b = body(json!({"max_tokens": 131072}));
        assert_eq!(normalize_params(&mut b, 131072), None);
        assert_eq!(b["max_tokens"], json!(131072));
    }

    #[test]
    fn never_exceeds_upstream_ceiling() {
        // 即便配置/document 把 cap 设得比上游更大，也必须收敛到真实上限。
        let mut b = body(json!({"max_tokens": 384000}));
        let capped = normalize_params(&mut b, 999_999);
        assert_eq!(capped, Some((384000, UPSTREAM_MAX_OUTPUT_TOKENS)));
        assert_eq!(b["max_tokens"], json!(UPSTREAM_MAX_OUTPUT_TOKENS));
    }

    #[test]
    fn maps_max_output_tokens_to_max_tokens() {
        let mut b = body(json!({"max_output_tokens": 1000}));
        normalize_params(&mut b, 65536);
        assert_eq!(b["max_tokens"], json!(1000));
        assert!(b.get("max_output_tokens").is_none());
    }

    #[test]
    fn maps_developer_role_to_system() {
        let mut b = body(json!({"messages": [
            {"role": "developer", "content": "你是助手"},
            {"role": "user", "content": "hi"}
        ]}));
        normalize_params(&mut b, 1000);
        assert_eq!(b["messages"][0]["role"], "system");
        // 其他角色不受影响
        assert_eq!(b["messages"][1]["role"], "user");
    }

    #[test]
    fn drops_reasoning_field() {
        let mut b = body(json!({"reasoning": {"effort": "max"}}));
        normalize_params(&mut b, 65536);
        assert!(b.get("reasoning").is_none());
    }

    #[test]
    fn detects_images_only_in_structured_content() {
        // 字符串里的 image_url 只是文本，不算图片入参。
        let text = body(json!({"messages":[{"role":"user","content":"image_url"}]}));
        assert_eq!(validate_image_input(&text), Ok(false));

        let image = body(json!({"messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
        ]}]}));
        assert_eq!(validate_image_input(&image), Ok(true));
    }

    #[test]
    fn reports_found_images_in_every_supported_shape() {
        let multi = body(json!({"messages": [
            {"role": "user", "content": "先看这张"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                {"type": "input_image", "image_url": "data:image/jpeg;base64,QUJD"}
            ]}
        ]}));
        assert_eq!(validate_image_input(&multi), Ok(true));

        // 没有 messages 也没有图片：不算带图，也不报错。
        let empty = body(json!({"messages": []}));
        assert_eq!(validate_image_input(&empty), Ok(false));
        let bare = body(json!({}));
        assert_eq!(validate_image_input(&bare), Ok(false));
    }

    #[test]
    fn validates_supported_native_image_payloads() {
        let b = body(json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "看"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
            {"type": "input_image", "image_url": "data:image/jpeg;base64,QUJD"},
            {"type": "image", "url": "data:image/webp;base64,QUJD"},
            {"type": "image", "source": {"type": "url", "url": "data:image/gif;base64,QUJD"}}
        ]}]}));
        assert_eq!(validate_image_input(&b), Ok(true));
    }

    #[test]
    fn rejects_remote_and_malformed_image_urls() {
        for url in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "data:image/png,QUJD",
            "data:text/plain;base64,QUJD",
            "data:image/png;base64,",
            "not-a-url",
        ] {
            let b = body(json!({"messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": url}}
            ]}]}));
            assert!(validate_image_input(&b).is_err(), "应拒绝该图片 URL：{url}");
        }
    }

    #[test]
    fn estimates_base64_size_for_limits() {
        assert_eq!(base64_decoded_len(""), 0);
        assert_eq!(base64_decoded_len("QUJD"), 3);
        assert_eq!(base64_decoded_len("QUJD\nRA=="), 4);
        assert!(base64_decoded_len(&"A".repeat(MAX_IMAGE_BYTES / 3 * 4 + 8)) > MAX_IMAGE_BYTES);
    }

    #[test]
    fn stops_scanning_once_the_limit_is_exceeded() {
        // 远超上限的 payload 只需扫到阈值，没必要全量遍历 27 MB。
        let huge = "A".repeat(MAX_IMAGE_BYTES / 3 * 4 * 4);
        assert!(base64_decoded_len_capped(&huge, MAX_IMAGE_BYTES) > MAX_IMAGE_BYTES);
        // 刚好在上限内仍要精确。
        assert_eq!(base64_decoded_len_capped("QUJD", MAX_IMAGE_BYTES), 3);
    }

    #[test]
    fn preserves_multimodal_image_content() {
        let content = json!([
            {"type": "text", "text": "描述图片"},
            {"type": "image_url", "image_url": {
                "url": "data:image/png;base64,AAA", "detail": "high"
            }}
        ]);
        let mut b = body(json!({"messages": [{"role": "user", "content": content.clone()}]}));
        normalize_params(&mut b, 1000);
        assert_eq!(b["messages"][0]["content"], content);
    }

    /// 线上真实事故：一次调用 5 个工具，第一个结果带截图，适配层在工具链
    /// 中间插了一条 user 图片消息，后面 4 个 tool 结果全变成孤儿。
    #[test]
    fn reorders_tool_results_split_by_interleaved_message() {
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "看这个仓库"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "Read_8", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Read_9", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Read_10", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Read_11", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "Bash_12", "type": "function", "function": {"name": "Bash", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "Read_8", "content": "shot"},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}}
            ]},
            {"role": "tool", "tool_call_id": "Read_9", "content": "a"},
            {"role": "tool", "tool_call_id": "Read_10", "content": "b"},
            {"role": "tool", "tool_call_id": "Read_11", "content": "c"},
            {"role": "tool", "tool_call_id": "Bash_12", "content": "d"}
        ]}));
        normalize_params(&mut b, 1000);

        let msgs = b["messages"].as_array().unwrap();
        // assistant 之后必须紧跟 5 条连续 tool 消息，且顺序与 tool_calls 一致
        assert_eq!(msgs[1]["role"], "assistant");
        let ids: Vec<&str> = msgs[2..7]
            .iter()
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["Read_8", "Read_9", "Read_10", "Read_11", "Bash_12"]);
        for msg in &msgs[2..7] {
            assert_eq!(msg["role"], "tool");
        }
        // 被打断的图片消息仍在，只是挪到了工具链之后
        let image_msg = msgs[7..]
            .iter()
            .find(|m| m["content"].is_array())
            .expect("图片消息不能丢");
        assert_eq!(image_msg["role"], "user");
        assert_eq!(msgs.len(), 8);
    }

    #[test]
    fn backfills_missing_tool_result_instead_of_dropping_call() {
        // 只回传了前两个结果：缺的补占位，不能删 tool_calls（否则又对不上号）
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "c2", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "c3", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c2", "content": "ok"}
        ]}));
        normalize_params(&mut b, 1000);

        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        let ids: Vec<&str> = msgs[2..5]
            .iter()
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        // 按 tool_calls 的声明顺序补齐，c2 的真实结果被提到它该在的位置
        assert_eq!(ids, ["c1", "c2", "c3"]);
        assert_eq!(msgs[3]["content"], "ok");
        assert!(msgs[2]["content"].as_str().unwrap().contains("未返回结果"));
    }

    #[test]
    fn demotes_orphan_tool_message_without_losing_content() {
        // 前面没有 assistant 认领这条 tool：上游会因未知 tool_call_id 拒绝，
        // 改成 user 保留内容。
        let mut b = body(json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "tool", "tool_call_id": "ghost", "content": "残缺结果"}
        ]}));
        normalize_params(&mut b, 1000);

        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "残缺结果");
        assert!(msgs[1].get("tool_call_id").is_none());
    }

    #[test]
    fn leaves_healthy_tool_chains_untouched() {
        let healthy = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            {"role": "user", "content": "谢谢"}
        ]);
        let mut b = body(json!({"messages": healthy.clone()}));
        normalize_params(&mut b, 1000);
        assert_eq!(
            Value::Array(b["messages"].as_array().unwrap().clone()),
            healthy
        );
    }
}
