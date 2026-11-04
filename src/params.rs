//! 请求参数归一化与上游错误映射。
//!
//! 这两块是 Python 版踩过坑之后补上的，Rust 版必须保持同样语义：
//! 1. 各客户端会用不同字段名表达输出长度（max_tokens / max_completion_tokens /
//!    max_output_tokens），上游只认 max_tokens 且超限时返回 HTTP 200 + 业务错误；
//! 2. 上游业务错误可能出现在 200 的 JSON 或 SSE 流里，必须翻译成可识别的形态。

use serde_json::{Map, Value};

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

    capped_now
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

/// 原生识图的图片入参校验。
///
/// 上游 `/api/v2/chat/completions` 只接受 base64 `data:` URL：远程
/// `http(s)://` 会被 `400 ModelArts.81001 unsupported image url` 拒绝。
/// 在发请求前给出可读错误，省掉一次注定失败的往返。
pub fn validate_image_input(body: &Map<String, Value>) -> Result<(), String> {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Ok(());
    };
    for message in messages {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if !matches!(
                part.get("type").and_then(Value::as_str),
                Some("image_url" | "input_image" | "image")
            ) {
                continue;
            }
            let url = image_url_of(part).ok_or("图片缺少 URL")?;
            check_image_url(url)?;
        }
    }
    Ok(())
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
            "上游只接受 base64 data URL，不接受远程图片 URL；请在客户端内联图片；\
             代理下载转码的可选方案见 docs/原生识图改造方案.md 第 5.4 节"
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
    let decoded = base64_decoded_len(payload);
    if decoded == 0 {
        return Err("图片内容为空".into());
    }
    if decoded > MAX_IMAGE_BYTES {
        return Err(format!(
            "单张图片解码后约 {decoded} 字节，超过上限 {MAX_IMAGE_BYTES} 字节"
        ));
    }
    Ok(())
}

/// 估算 base64 解码后的字节数（不计空白与补位），用于大小校验。
fn base64_decoded_len(payload: &str) -> usize {
    let chars = payload
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
        .count();
    chars.saturating_mul(3) / 4
}

pub fn has_image_input(body: &Map<String, Value>) -> bool {
    body.get("messages")
        .and_then(|v| v.as_array())
        .map(|messages| {
            messages.iter().any(|message| {
                message
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|parts| {
                        parts.iter().any(|part| {
                            matches!(
                                part.get("type").and_then(|v| v.as_str()),
                                Some("image_url" | "input_image" | "image")
                            )
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
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
        let text = body(json!({"messages":[{"role":"user","content":"image_url"}]}));
        assert!(!has_image_input(&text));
        let image = body(json!({"messages":[{"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
        ]}]}));
        assert!(has_image_input(&image));
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
        assert!(validate_image_input(&b).is_ok());
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
}
