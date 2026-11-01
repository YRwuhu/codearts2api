//! CodeArts 网页版附件 + CloudAgent 会话通道。
//!
//! `supports_images` 描述的是 `/api/v2/chat/completions` 的内嵌图片能力。
//! 网页上传图片走的是另一条链路：上传附件后用同一账号签名创建 CloudAgent
//! 会话并提交任务。`Agent-Type: CodeBase` 是该服务路由所必需的请求头。

use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::chat::{ApiError, Outcome, StreamEvent};
use crate::models::{Catalog, BASE_URL};
use crate::web::WebState;

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub struct ImageAttachment {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// 将最后一条用户消息中的文字和 base64 图片转换为本轮网页任务。
/// CloudAgent 会话由当前请求独立创建，历史消息不会被伪造为已存在的会话。
pub fn input_from_messages(body: &Value) -> Result<(String, Vec<ImageAttachment>), String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("附件通道需要 messages")?;
    let mut content = Vec::new();
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .ok_or("附件请求缺少用户消息")?;
    if let Some(text) = last_user.get("content").and_then(Value::as_str) {
        content.push(text.to_string());
    }
    if let Some(parts) = last_user.get("content").and_then(Value::as_array) {
        for part in parts {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("text" | "input_text")
            ) {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    content.push(text.to_string());
                }
            }
        }
    }
    let mut images = Vec::new();
    if let Some(parts) = last_user.get("content").and_then(Value::as_array) {
        for part in parts {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("image_url" | "input_image" | "image")
            ) {
                images.push(decode_image(part, images.len())?);
            }
        }
    }
    if images.is_empty() {
        return Err("附件通道目前只支持最后一条用户消息中的 base64 图片".into());
    }
    Ok((content.join("\n\n"), images))
}

fn user_prompt(content: &str, images: &[Value]) -> String {
    let mut lines: Vec<String> = images
        .iter()
        .filter_map(|image| image["filename"].as_str().map(|name| format!("# {name}")))
        .collect();
    lines.push(content.to_string());
    lines.join("\n")
}

fn context_part(content: &str, images: &[Value]) -> String {
    let mut parts: Vec<Value> = images
        .iter()
        .map(|image| {
            json!({
                "type": "file",
                "name": image["filename"],
                "mime": image["mime"],
            })
        })
        .collect();
    if !content.trim().is_empty() {
        parts.push(json!({"type": "text", "content": content}));
    }
    serde_json::to_string(&parts).unwrap_or_else(|_| "[]".into())
}

fn decode_image(part: &Value, index: usize) -> Result<ImageAttachment, String> {
    let url = part
        .get("image_url")
        .and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            Value::Object(_) => v.get("url").and_then(Value::as_str),
            _ => None,
        })
        .or_else(|| part.get("url").and_then(Value::as_str))
        .ok_or("图片缺少 URL")?;
    let data = url
        .strip_prefix("data:")
        .ok_or("附件通道暂不支持远程图片 URL，请使用 base64 data URL")?;
    let (meta, payload) = data.split_once(',').ok_or("图片 data URL 格式无效")?;
    if !meta.split(';').any(|v| v.eq_ignore_ascii_case("base64")) {
        return Err("图片 data URL 必须使用 base64 编码".into());
    }
    let mime = meta.split(';').next().unwrap_or("").to_ascii_lowercase();
    let extension = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => return Err(format!("不支持的图片 MIME: {mime}")),
    };
    let compact: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|_| "图片 base64 解码失败".to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!("图片大小须为 1 字节至 {MAX_IMAGE_BYTES} 字节"));
    }
    Ok(ImageAttachment {
        filename: format!("image-{}.{}", index + 1, extension),
        mime,
        bytes,
    })
}

async fn response_detail(response: reqwest::Response) -> (reqwest::StatusCode, String) {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("error_msg")
                .or_else(|| v.get("error"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.chars().take(500).collect());
    (status, message)
}

/// 在发起模型任务前先创建会话，失败时仍可走现有视觉模型回退。
pub async fn prepare_session(catalog: &Catalog) -> Result<String, String> {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/sessions");
    let headers = catalog
        .signed_request("POST", &url, b"{}", &[("agent-type", "CodeBase")])
        .await;
    let mut request = catalog
        .http_client()
        .post(url)
        .timeout(Duration::from_secs(30))
        .body("{}");
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("CloudAgent 创建会话 HTTP {status}: {text}"));
    }
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("result")
                .and_then(|r| r.get("session_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| "CloudAgent 会话响应缺少 session_id".into())
}

async fn upload(
    http: &reqwest::Client,
    catalog: &Catalog,
    image: ImageAttachment,
) -> Result<Value, String> {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/attachments");
    let part = reqwest::multipart::Part::bytes(image.bytes)
        .file_name(image.filename.clone())
        .mime_str(&image.mime)
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().part("file", part);
    let boundary = form.boundary().to_string();
    let body = futures_util::TryStreamExt::try_concat(futures_util::StreamExt::map(
        form.into_stream(),
        |chunk| chunk.map(|bytes| bytes::BytesMut::from(bytes.as_ref())),
    ))
    .await
    .map_err(|e| format!("读取附件上传数据失败: {e}"))?
    .freeze();
    let content_type = format!("multipart/form-data; boundary={boundary}");
    let extra = [
        ("content-type", content_type.as_str()),
        ("accept", "application/json"),
        ("x-requested-with", "XMLHttpRequest"),
        ("x-codearts-doer-scenario-type", "web"),
        ("x-ot-client-type", "web"),
    ];
    let headers = catalog.signed_request("POST", &url, &body, &extra).await;
    let mut request = http.post(url).timeout(Duration::from_secs(120)).body(body);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| format!("附件上传未返回 JSON（HTTP {status}）"))?;
    if !status.is_success() {
        return Err(format!("附件上传 HTTP {status}: {text}"));
    }
    value
        .get("result")
        .filter(|r| r.get("attachment_id").and_then(Value::as_str).is_some())
        .cloned()
        .ok_or_else(|| format!("附件上传失败: {text}"))
}

pub fn task_body(content: &str, model: &str, attachments: &[Value]) -> Value {
    json!({
        "content": user_prompt(content, attachments),
        "model_id": model,
        "repos": [],
        "agent": "build",
        "attachments": attachments,
        "context_part": context_part(content, attachments),
    })
}

/// 发送带附件任务，并把 CloudAgent SSE 统一转换为现有 OpenAI 事件流。
pub async fn execute(
    state: &WebState,
    catalog: &Catalog,
    model: &str,
    body: &Value,
    client_stream: bool,
    session_id: &str,
) -> Result<Outcome, ApiError> {
    let started = std::time::Instant::now();
    let (content, images) = input_from_messages(body).map_err(|message| {
        ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "cloudagent_invalid_input",
            message,
        )
    })?;
    let mut attachments = Vec::new();
    for image in images {
        let uploaded = upload(&state.http, catalog, image)
            .await
            .map_err(ApiError::bad_request)?;
        attachments.push(json!({
            "attachment_id": uploaded["attachment_id"],
            "filename": uploaded["filename"],
            "mime": uploaded["mime"],
        }));
    }
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/sessions/{session_id}/messages");
    let payload = serde_json::to_vec(&task_body(&content, model, &attachments))
        .map_err(|e| ApiError::bad_request(format!("序列化附件任务失败: {e}")))?;
    let extra = [
        ("accept", "text/event-stream"),
        ("agent-type", "CodeBase"),
        ("x-requested-with", "XMLHttpRequest"),
        ("x-codearts-doer-scenario-type", "web"),
        ("x-ot-client-type", "web"),
    ];
    let headers = catalog.signed_request("POST", &url, &payload, &extra).await;
    let mut request = state
        .http
        .post(url)
        .timeout(Duration::from_secs(600))
        .body(payload);
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| ApiError::unavailable(format!("CloudAgent 任务请求失败: {e}")))?;
    if !response.status().is_success() {
        let (status, detail) = response_detail(response).await;
        return Err(ApiError::new(
            axum::http::StatusCode::from_u16(status.as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
            "cloudagent_error",
            detail,
        ));
    }
    let raw = response
        .bytes()
        .await
        .map_err(|e| ApiError::unavailable(format!("读取 CloudAgent 任务流失败: {e}")))?;
    let result = parse_events(&raw, model, session_id).map_err(ApiError::bad_request)?;
    state
        .usage
        .record(crate::usage::UsageRecord::from_upstream(
            model,
            catalog.account_id(),
            started.elapsed().as_millis() as i64,
            client_stream,
            result.get("usage"),
        ))
        .await;
    if let Some(total) = result
        .get("usage")
        .and_then(|usage| usage.get("total_tokens"))
        .and_then(Value::as_i64)
    {
        state
            .gate(catalog.account_id(), state.config().concurrency)
            .await
            .add_daily_tokens(total)
            .await;
    }
    if client_stream {
        let chunk = crate::chat::chat_chunk_from_completion(&result);
        Ok(Outcome::Stream(Box::pin(futures_util::stream::iter(vec![
            Ok(StreamEvent::Chunk(chunk)),
            Ok(StreamEvent::Done),
        ]))))
    } else {
        Ok(Outcome::Json(result))
    }
}

pub fn parse_events(raw: &[u8], model: &str, session_id: &str) -> Result<Value, String> {
    let text = String::from_utf8_lossy(raw);
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut terminal_error = None;
    let mut usage = None;
    let mut done = false;
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let mut event = "";
        let mut data = String::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.trim());
            }
        }
        if data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let error_text = value
            .get("error")
            .or_else(|| value.get("error_msg"))
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty());
        if event == "error" || error_text.is_some() {
            terminal_error = value
                .get("error")
                .or_else(|| value.get("error_msg"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(data.clone()));
            continue;
        }
        if event == "tool_result" && value.get("status").and_then(Value::as_str) == Some("error") {
            terminal_error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some("CloudAgent 工具执行失败".into()));
            continue;
        }
        if event == "thought" {
            if let Some(s) = value.get("content").and_then(Value::as_str) {
                reasoning.push_str(s);
            }
        }
        if event == "done" {
            done = true;
            usage = value.get("tokens").map(|tokens| {
                json!({
                    "prompt_tokens": tokens["input"],
                    "completion_tokens": tokens["output"],
                    "total_tokens": tokens["total"],
                    "completion_tokens_details": {"reasoning_tokens": tokens["reasoning"]},
                })
            });
        }
        if event == "message" {
            if let Some(s) = value.get("content").and_then(Value::as_str) {
                content.push_str(s);
            }
        }
    }
    if let Some(error) = terminal_error {
        return Err(error);
    }
    if content.is_empty() {
        return Err("CloudAgent 未返回文本内容；任务可能仍在处理或未完成".into());
    }
    if !done {
        return Err("CloudAgent 任务在完成事件前结束，响应可能不完整".into());
    }
    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    Ok(json!({
        "id": format!("chatcmpl-{session_id}"),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_last_user_base64_image() {
        let body = json!({"messages": [{"role":"user", "content":[
            {"type":"text", "text":"描述图片"},
            {"type":"image_url", "image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}
        ]}]});
        let (text, images) = input_from_messages(&body).unwrap();
        assert!(text.contains("描述图片"));
        assert_eq!(images[0].filename, "image-1.png");
        assert_eq!(images[0].mime, "image/png");
        assert!(!images[0].bytes.is_empty());
    }

    #[test]
    fn rejects_remote_images_instead_of_fetching_them() {
        let body = json!({"messages": [{"role":"user", "content":[
            {"type":"image_url", "image_url":{"url":"https://example.com/a.png"}}
        ]}]});
        assert!(input_from_messages(&body).is_err());
    }

    #[test]
    fn builds_web_task_with_attachment_references() {
        let payload = task_body(
            "describe",
            "deepseek-v4.1-flash-1017",
            &[json!({
                "attachment_id":"a1", "filename":"a.png", "mime":"image/png"
            })],
        );
        assert_eq!(payload["model_id"], "deepseek-v4.1-flash-1017");
        assert_eq!(payload["attachments"][0]["attachment_id"], "a1");
        assert!(payload.get("messages").is_none());
    }

    #[test]
    fn parses_text_deltas_and_ignores_status() {
        let raw = b"event: status\ndata: {\"status\":\"processing\"}\n\nevent: message\ndata: {\"content\":\"A flag\"}\n\nevent: message\ndata: {\"content\":\" on a hill.\"}\n\nevent: done\ndata: {}\n\n";
        let result = parse_events(raw, "deepseek-v4.1-flash-1017", "s1").unwrap();
        assert_eq!(
            result["choices"][0]["message"]["content"],
            "A flag on a hill."
        );
    }

    #[test]
    fn parses_real_message_events_and_usage() {
        let raw = b"event: thought\ndata: {\"content\":\"look\"}\n\nevent: message\ndata: {\"content\":\"A red square.\"}\n\nevent: done\ndata: {\"tokens\":{\"input\":10,\"output\":4,\"reasoning\":2,\"total\":14}}\n\n";
        let result = parse_events(raw, "deepseek-v4.1-flash-1017", "s1").unwrap();
        assert_eq!(result["choices"][0]["message"]["content"], "A red square.");
        assert_eq!(result["choices"][0]["message"]["reasoning_content"], "look");
        assert_eq!(result["usage"]["total_tokens"], 14);
    }

    #[test]
    fn rejects_truncated_task_before_done() {
        let raw = b"event: message\ndata: {\"content\":\"partial\"}\n\n";
        assert!(parse_events(raw, "m", "s").is_err());
    }

    #[test]
    fn rejects_failed_image_tool() {
        let raw = b"event: tool_result\ndata: {\"status\":\"error\",\"error\":\"image not found\"}\n\nevent: message\ndata: {\"content\":\"cannot see it\"}\n\nevent: done\ndata: {}\n\n";
        assert!(parse_events(raw, "m", "s")
            .unwrap_err()
            .contains("image not found"));
    }

    #[test]
    fn surfaces_terminal_task_errors() {
        let raw = b"event: error\ndata: {\"error\":\"quota exhausted\"}\n\n";
        assert!(parse_events(raw, "m", "s")
            .unwrap_err()
            .contains("quota exhausted"));
    }
}
