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

    capped_now
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
    fn drops_reasoning_field() {
        let mut b = body(json!({"reasoning": {"effort": "max"}}));
        normalize_params(&mut b, 65536);
        assert!(b.get("reasoning").is_none());
    }

}
