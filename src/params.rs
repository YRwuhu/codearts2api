//! 请求参数归一化与上游错误映射。
//!
//! 这两块是 Python 版踩过坑之后补上的，Rust 版必须保持同样语义：
//! 1. 各客户端会用不同字段名表达输出长度（max_tokens / max_completion_tokens /
//!    max_output_tokens），上游只认 max_tokens 且超限时返回 HTTP 200 + 业务错误；
//! 2. 上游业务错误可能出现在 200 的 JSON 或 SSE 流里，必须翻译成可识别的形态。

use serde_json::{Map, Value};

/// 把 max_* 系列字段统一成 max_tokens，并按模型上限裁剪。
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

    let mut capped_now = None;
    if let Some(orig) = original {
        let capped = orig.min(cap);
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

/// 业务错误码通常带 `.400`/`.429` 后缀，据此给出真实 HTTP 状态码。
pub fn error_status(value: &Value) -> u16 {
    let code = value
        .get("error_code")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    for (suffix, status) in [
        (".400", 400u16),
        (".401", 401),
        (".403", 403),
        (".404", 404),
        (".429", 429),
    ] {
        if code.ends_with(suffix) {
            return status;
        }
    }
    502
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

    #[test]
    fn error_status_from_suffix() {
        assert_eq!(error_status(&json!({"error_code": "InferHub.001001005.400"})), 400);
        assert_eq!(error_status(&json!({"error_code": "SomethingElse"})), 502);
    }
}
