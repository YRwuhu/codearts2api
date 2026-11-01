//! CodeArts 网页版附件 + CloudAgent 会话通道。
//!
//! `supports_images` 描述的是 `/api/v2/chat/completions` 的内嵌图片能力。
//! 网页上传图片走的是另一条链路：上传附件后用同一账号签名创建 CloudAgent
//! 会话并提交任务。`Agent-Type: CodeBase` 是该服务路由所必需的请求头。

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::chat::{ApiError, EventStream, Outcome, StreamEvent};
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

async fn signed_request(
    catalog: &Catalog,
    method: &str,
    url: &str,
    body: &[u8],
    accept: &str,
) -> Vec<(String, String)> {
    let session_id = crate::concurrency::account_session_id(&catalog.account().await.session_seed());
    let extra = [
        ("accept", accept),
        ("agent-type", "CodeBase"),
        ("x-requested-with", "XMLHttpRequest"),
        ("x-codearts-doer-scenario-type", "web"),
        ("x-ot-client-type", "web"),
        ("x-ot-session-id", session_id.as_str()),
        ("user-session-id", session_id.as_str()),
    ];
    catalog.signed_request(method, url, body, &extra).await
}

/// 在发起模型任务前先创建会话，失败时仍可走现有视觉模型回退。
pub async fn prepare_session(catalog: &Catalog) -> Result<String, String> {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/sessions");
    let headers = signed_request(catalog, "POST", &url, b"{}", "application/json").await;
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

/// 关闭 CloudAgent 会话。
///
/// 上游按账号限制**同时存在的会话数**（实测 3 个）。`prepare_session` 每轮
/// 都要新建一个会话，若不回收，第 4 次带图请求起就会撞上限并回退视觉模型。
/// `DELETE /cloudagent/sessions/{id}` 是网页版关闭会话的接口，返回 200。
async fn close_session(catalog: &Catalog, session_id: &str) {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/sessions/{session_id}");
    let headers = signed_request(catalog, "DELETE", &url, b"", "application/json").await;
    let mut request = catalog
        .http_client()
        .delete(url)
        .timeout(Duration::from_secs(30));
    for (key, value) in headers {
        request = request.header(key, value);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => println!(
            "[cloudagent] 关闭会话 {session_id} 失败：HTTP {}",
            response.status()
        ),
        Err(error) => println!("[cloudagent] 关闭会话 {session_id} 失败：{error}"),
    }
}

/// 会话的 RAII 回收句柄。
///
/// 用 Drop 而不是在每个 `?` 前手写 close：`execute` 里有多条提前返回路径
/// （上传失败、任务失败、解析失败），漏掉一处就重新泄漏一个会话。
pub struct SessionGuard {
    catalog: Arc<Catalog>,
    id: String,
}

impl SessionGuard {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let catalog = self.catalog.clone();
        let id = self.id.clone();
        // Drop 不能 await：在运行时外起一个短任务收尾。请求已完成，
        // 这里失败也只是多留一个会话，不该影响已经返回给客户端的结果。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { close_session(&catalog, &id).await });
        }
    }
}

/// 打开一个用完即还的会话。
pub async fn open_session(catalog: &Arc<Catalog>) -> Result<SessionGuard, String> {
    let id = prepare_session(catalog).await?;
    Ok(SessionGuard {
        catalog: catalog.clone(),
        id,
    })
}

/// 回收此前泄漏的会话，最多保留 `keep` 个。
///
/// 老版本没有关闭逻辑，账号上会积压会话，导致新请求一上来就撞
/// `并发会话数已达上限`。启动时清理一次即可恢复。
///
/// 注意：上游的"会话数"统计**所有未删除的会话**，不只是 `ACTIVE`。
/// 实测账号上 18 个会话、只有 1 个 ACTIVE 时仍报上限，所以这里不能
/// 按状态过滤，必须连 `IDLE` 一起删。
pub async fn reclaim_sessions(catalog: &Catalog, keep: usize) {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/sessions?page_num=1&page_size=100");
    let headers = signed_request(catalog, "GET", &url, b"", "application/json").await;
    let mut request = catalog
        .http_client()
        .get(url)
        .timeout(Duration::from_secs(30));
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let Ok(response) = request.send().await else {
        return;
    };
    if !response.status().is_success() {
        return;
    }
    let Ok(text) = response.text().await else {
        return;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(sessions) = value.get("result").and_then(Value::as_array) else {
        return;
    };
    if sessions.len() <= keep {
        return;
    }
    let mut closed = 0usize;
    for session in &sessions[..sessions.len() - keep] {
        if let Some(id) = session.get("session_id").and_then(Value::as_str) {
            close_session(catalog, id).await;
            closed += 1;
        }
    }
    if closed > 0 {
        println!(
            "[cloudagent] 账号 {} 回收了 {closed} 个遗留会话",
            catalog.account_id()
        );
    }
}


/// 上传图片附件，返回上游的 `attachment_id`。
///
/// `content-type` 必须参与签名（multipart 的 boundary 就在里面），
/// 否则网关报 `APIG.0301 verify ak sk signature fail`。
async fn upload(
    http: &reqwest::Client,
    catalog: &Catalog,
    bytes: Vec<u8>,
    filename: &str,
    mime: &str,
) -> Result<Value, String> {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/attachments");
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str(mime)
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
        ("agent-type", "CodeBase"),
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

async fn session_messages(catalog: &Catalog, session_id: &str) -> Result<Vec<Value>, String> {
    let url = format!(
        "{BASE_URL}/codebaseservice/v1/cloudagent/sessions/{session_id}/messages?page_num=1&page_size=200"
    );
    let headers = signed_request(catalog, "GET", &url, b"", "application/json").await;
    let mut request = catalog
        .http_client()
        .get(url)
        .timeout(Duration::from_secs(30));
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("读取 CloudAgent 附件映射 HTTP {status}: {text}"));
    }
    let value = serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?;
    Ok(value
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn image_hint(messages: &[Value], filename: &str) -> Option<(String, String)> {
    let needle = format!("[ImageName: {filename}]");
    for message in messages.iter().rev() {
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        if !content.contains(&needle) || !content.contains("[ImageId: ") {
            continue;
        }
        let id = content
            .split("[ImageId: ")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .unwrap_or("")
            .trim()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let path = content
            .split("[ImagePath: ")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .unwrap_or("")
            .trim()
            .to_string();
        return Some((id, path));
    }
    None
}

fn attachment_retry_prompt(content: &str, hints: &[(String, String)]) -> String {
    let mut text = format!("{content}\n\n请读取本轮已经上传的图片并回答上述问题。");
    let ids: Vec<&str> = hints.iter().map(|(id, _)| id.as_str()).collect();
    let paths: Vec<&str> = hints
        .iter()
        .map(|(_, path)| path.as_str())
        .filter(|path| !path.is_empty())
        .collect();
    if !ids.is_empty() {
        text.push_str(&format!(
            "这些图片在会话中的 ImageId 为：{}。调用 analyzeImage 时，优先将这些 ID 传给 imageId，不要把上传 attachment_id 或文件名当作 ImageId。",
            ids.join(", ")
        ));
    }
    if !paths.is_empty() {
        text.push_str(&format!(
            "如果 imageId 无法使用，使用会话返回的 imagePath：{}。不要搜索或猜测其他路径。",
            paths.join(", ")
        ));
    }
    text
}

/// 发起 CloudAgent 任务，返回**未读取**的响应，交给调用方逐块消费。
///
/// 不要在这里 `.bytes()`：CloudAgent 一轮任务 20–40 秒、上百帧 SSE，
/// 收完再返回会让客户端整段时间收不到任何字节（首字延迟 = 任务总时长）。
async fn start_task(
    state: &WebState,
    catalog: &Catalog,
    model: &str,
    content: &str,
    attachments: &[Value],
    session_id: &str,
) -> Result<reqwest::Response, ApiError> {
    let url = format!("{BASE_URL}/codebaseservice/v1/cloudagent/sessions/{session_id}/messages");
    let payload = serde_json::to_vec(&task_body(content, model, attachments))
        .map_err(|e| ApiError::bad_request(format!("序列化附件任务失败: {e}")))?;
    let headers = signed_request(catalog, "POST", &url, &payload, "text/event-stream").await;
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
    Ok(response)
}

async fn run_task(
    state: &WebState,
    catalog: &Catalog,
    model: &str,
    content: &str,
    attachments: &[Value],
    session_id: &str,
) -> Result<Vec<u8>, ApiError> {
    start_task(state, catalog, model, content, attachments, session_id)
        .await?
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|e| ApiError::unavailable(format!("读取 CloudAgent 任务流失败: {e}")))
}

/// 非流式路径：收完整轮任务；首次失败会读会话里的 ImageId 再试一次。
async fn collect(
    state: &WebState,
    catalog: &Catalog,
    model: &str,
    content: &str,
    attachments: &[Value],
    session_id: &str,
) -> Result<Value, ApiError> {
    let raw = run_task(state, catalog, model, content, attachments, session_id).await?;
    let mut result = parse_events(&raw, model, session_id);
    if let Err(error) = &result {
        // 第一次失败的原因通常比"重试也失败"更有诊断价值，别吞掉。
        println!("[cloudagent] {model}: 图像任务未成功（{error}），尝试会话注册的 ImageId");
        let messages = session_messages(catalog, session_id)
            .await
            .map_err(ApiError::bad_request)?;
        let hints: Vec<(String, String)> = attachments
            .iter()
            .filter_map(|attachment| {
                image_hint(&messages, attachment["filename"].as_str().unwrap_or(""))
            })
            .collect();
        if !hints.is_empty() {
            println!("[cloudagent] {model}: 使用会话注册的 ImageId 重试图片任务");
            let retry = attachment_retry_prompt(content, &hints);
            let raw = run_task(state, catalog, model, &retry, attachments, session_id).await?;
            result = parse_events(&raw, model, session_id);
        }
    }
    result.map_err(ApiError::bad_request)
}

/// 从缓冲区取出一个 SSE block（以空行结束）。
fn take_block(buffer: &mut Vec<u8>) -> Option<String> {
    let pos = buffer.windows(2).position(|w| w == b"\n\n")?;
    let block: Vec<u8> = buffer.drain(..pos + 2).collect();
    Some(String::from_utf8_lossy(&block[..pos]).replace("\r\n", "\n"))
}

/// 流式路径：把 CloudAgent 的 `event: message` 逐帧转成 OpenAI delta。
///
/// 会话句柄由流持有，直到这一轮结束才关闭；`execute` 一返回就 drop
/// 会在任务还在跑的时候把会话删掉。
fn stream_task(
    state: WebState,
    catalog: Arc<Catalog>,
    model: String,
    content: String,
    attachments: Vec<Value>,
    session: SessionGuard,
    started: std::time::Instant,
) -> EventStream {
    Box::pin(async_stream::stream! {
        use futures_util::StreamExt;

        let session_id = session.id().to_string();
        let id = format!("chatcmpl-{session_id}");
        let created = chrono::Utc::now().timestamp();
        let make_chunk = |delta: Value, finish: Option<&str>, usage: Option<&Value>| {
            let mut chunk = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            });
            if let Some(u) = usage {
                chunk["usage"] = u.clone();
            }
            chunk
        };

        // 先发 role 帧，让客户端立刻拿到首个字节，不必等到模型出字。
        yield Ok(StreamEvent::Chunk(make_chunk(
            json!({"role": "assistant", "content": ""}),
            None,
            None,
        )));

        let mut acc = TaskAccumulator::default();
        let mut emitted = false;
        let mut failure: Option<ApiError> = None;

        match start_task(&state, &catalog, &model, &content, &attachments, &session_id).await {
            Ok(response) => {
                let mut bytes = response.bytes_stream();
                let mut buffer: Vec<u8> = Vec::new();
                loop {
                    match bytes.next().await {
                        Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                        Some(Err(e)) => {
                            failure = Some(ApiError::unavailable(format!(
                                "读取 CloudAgent 任务流失败: {e}"
                            )));
                            break;
                        }
                        None => break,
                    }
                    while let Some(block) = take_block(&mut buffer) {
                        let delta = acc.feed(&block);
                        if !delta.is_empty() {
                            emitted = true;
                            yield Ok(StreamEvent::Chunk(make_chunk(
                                json!({"content": delta}),
                                None,
                                None,
                            )));
                        }
                    }
                }
                if failure.is_none() {
                    if let Some(block) = take_block(&mut buffer) {
                        let delta = acc.feed(&block);
                        if !delta.is_empty() {
                            emitted = true;
                            yield Ok(StreamEvent::Chunk(make_chunk(
                                json!({"content": delta}),
                                None,
                                None,
                            )));
                        }
                    }
                }
            }
            Err(error) => failure = Some(error),
        }

        let usage = acc.usage.clone();

        // 还没有任何正文吐出去时，才有资格改走 ImageId 重试，否则会重复输出。
        if failure.is_none() && !emitted && acc.preview_error(&model, &session_id).is_some() {
            if let Ok(messages) = session_messages(&catalog, &session_id).await {
                let hints: Vec<(String, String)> = attachments
                    .iter()
                    .filter_map(|attachment| {
                        image_hint(&messages, attachment["filename"].as_str().unwrap_or(""))
                    })
                    .collect();
                if !hints.is_empty() {
                    println!("[cloudagent] {model}: 使用会话注册的 ImageId 流式重试");
                    let retry = attachment_retry_prompt(&content, &hints);
                    acc = TaskAccumulator::default();
                    match start_task(&state, &catalog, &model, &retry, &attachments, &session_id).await {
                        Ok(response) => {
                            let mut bytes = response.bytes_stream();
                            let mut buffer: Vec<u8> = Vec::new();
                            loop {
                                match bytes.next().await {
                                    Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                                    Some(Err(e)) => {
                                        failure = Some(ApiError::unavailable(format!(
                                            "读取 CloudAgent 重试流失败: {e}"
                                        )));
                                        break;
                                    }
                                    None => break,
                                }
                                while let Some(block) = take_block(&mut buffer) {
                                    let delta = acc.feed(&block);
                                    if !delta.is_empty() {
                                        emitted = true;
                                        yield Ok(StreamEvent::Chunk(make_chunk(
                                            json!({"content": delta}),
                                            None,
                                            None,
                                        )));
                                    }
                                }
                            }
                        }
                        Err(error) => failure = Some(error),
                    }
                }
            }
        }

        if let Some(error) = failure {
            state
                .usage
                .record(crate::usage::UsageRecord::failed(
                    &model,
                    catalog.account_id(),
                    started.elapsed().as_millis() as i64,
                    true,
                    format!("{}: {}", error.code, error.message),
                ))
                .await;
            yield Err(error);
            return;
        }

        let usage = if emitted { acc.usage.clone().or(usage) } else { usage };
        let doom = acc.content.contains("已检测到 doom loop");
        let outcome = if emitted {
            // 已经逐帧发过正文，这里只判定终止条件，不再重复输出内容。
            if let Some(error) = acc
                .terminal_error
                .clone()
                .filter(|_| !acc.successful_image_analysis)
            {
                Err(error)
            } else if !acc.done {
                Err("CloudAgent 任务在完成事件前结束，响应可能不完整".to_string())
            } else {
                Ok(())
            }
        } else {
            acc.finish(&model, &session_id).map(|_| ())
        };

        let _completion_session = session;

        match outcome {
            Ok(()) if !doom => {
                state
                    .usage
                    .record(crate::usage::UsageRecord::from_upstream(
                        &model,
                        catalog.account_id(),
                        started.elapsed().as_millis() as i64,
                        true,
                        usage.as_ref(),
                    ))
                    .await;
                if let Some(total) = usage
                    .as_ref()
                    .and_then(|u| u.get("total_tokens"))
                    .and_then(Value::as_i64)
                {
                    state
                        .gate(catalog.account_id(), state.config().concurrency)
                        .await
                        .add_daily_tokens(total)
                        .await;
                }
                yield Ok(StreamEvent::Chunk(make_chunk(
                    json!({}),
                    Some("stop"),
                    usage.as_ref(),
                )));
                yield Ok(StreamEvent::Done);
            }
            Ok(()) => {
                yield Err(ApiError::bad_request(
                    "CloudAgent 图像任务重复调用被中止，使用视觉模型回退",
                ));
            }
            Err(message) => {
                state
                    .usage
                    .record(crate::usage::UsageRecord::failed(
                        &model,
                        catalog.account_id(),
                        started.elapsed().as_millis() as i64,
                        true,
                        message.clone(),
                    ))
                    .await;
                yield Err(ApiError::bad_request(message));
            }
        }
    })
}

/// 发送带附件任务，并把 CloudAgent SSE 统一转换为现有 OpenAI 事件流。
pub async fn execute(
    state: &WebState,
    catalog: Arc<Catalog>,
    model: &str,
    body: &Value,
    client_stream: bool,
    session: SessionGuard,
) -> Result<Outcome, ApiError> {
    let started = std::time::Instant::now();
    let session_id = session.id().to_string();
    println!("[cloudagent] {model}: 使用附件 + CloudAgent 会话 {session_id}");
    let (content, images) = input_from_messages(body).map_err(|message| {
        ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "cloudagent_invalid_input",
            message,
        )
    })?;
    let mut attachments = Vec::new();
    for image in &images {
        let uploaded = upload(
            &state.http,
            &catalog,
            image.bytes.clone(),
            &image.filename,
            &image.mime,
        )
        .await
        .map_err(ApiError::bad_request)?;
        attachments.push(json!({
            "attachment_id": uploaded["attachment_id"],
            "filename": uploaded["filename"],
            "mime": uploaded["mime"],
        }));
    }

    if client_stream {
        return Ok(Outcome::Stream(stream_task(
            state.clone(),
            catalog,
            model.to_string(),
            content,
            attachments,
            session,
            started,
        )));
    }

    let result = collect(state, &catalog, model, &content, &attachments, &session_id).await?;
    drop(session);
    if result["choices"][0]["message"]["content"]
        .as_str()
        .is_some_and(|content| content.contains("已检测到 doom loop"))
    {
        return Err(ApiError::bad_request(
            "CloudAgent 图像任务重复调用被中止，使用视觉模型回退",
        ));
    }
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
    Ok(Outcome::Json(result))
}

fn response_usage(value: &Value) -> Option<Value> {
    let tokens = value.get("tokens")?;
    Some(json!({
        "prompt_tokens": tokens["input"],
        "completion_tokens": tokens["output"],
        "total_tokens": tokens["total"],
        "completion_tokens_details": {"reasoning_tokens": tokens["reasoning"]},
    }))
}

/// 逐个 SSE block 累积 CloudAgent 任务状态。
///
/// 流式与聚合两条路径共用同一套解析逻辑：流式逐块 `feed` 并立刻把正文
/// 转成 OpenAI delta，聚合则全部喂完后 `finish`。
#[derive(Default)]
struct TaskAccumulator {
    content: String,
    reasoning: String,
    image_analysis: Option<String>,
    terminal_error: Option<String>,
    successful_image_analysis: bool,
    usage: Option<Value>,
    done: bool,
}

impl TaskAccumulator {
    /// 喂一个完整 block（`event:` + `data:` 若干行），返回本次新增的正文增量。
    fn feed(&mut self, block: &str) -> String {
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
            return String::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return String::new();
        };
        let error_text = value
            .get("error")
            .or_else(|| value.get("error_msg"))
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty());
        if event == "error" || error_text.is_some() {
            self.terminal_error = error_text.map(str::to_string).or_else(|| Some(data.clone()));
            return String::new();
        }
        if event == "tool_result" {
            if value.get("tool").and_then(Value::as_str) == Some("analyzeImage")
                && value.get("status").and_then(Value::as_str) == Some("completed")
                && value
                    .get("result")
                    .and_then(Value::as_str)
                    .is_some_and(|result| !result.trim().is_empty())
            {
                self.successful_image_analysis = true;
                self.image_analysis = value
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if value.get("status").and_then(Value::as_str) == Some("error") {
                self.terminal_error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("CloudAgent 工具执行失败".into()));
                return String::new();
            }
        }
        if event == "thought" {
            if let Some(s) = value.get("content").and_then(Value::as_str) {
                self.reasoning.push_str(s);
            }
        }
        if event == "done" {
            self.done = true;
            self.usage = response_usage(&value);
        }
        if event == "message" {
            if let Some(s) = value.get("content").and_then(Value::as_str) {
                self.content.push_str(s);
                return s.to_string();
            }
        }
        String::new()
    }

    /// 预测 `finish` 会不会失败，用于决定是否值得改走 ImageId 重试。
    ///
    /// 与 `finish` 同源但不消耗 `self`，因为后面还要继续用它。
    fn preview_error(&self, _model: &str, _session_id: &str) -> Option<String> {
        if let Some(error) = self
            .terminal_error
            .clone()
            .filter(|_| !self.successful_image_analysis)
        {
            return Some(error);
        }
        if self.content.is_empty() && self.image_analysis.is_none() {
            return Some("CloudAgent 未返回文本内容；任务可能仍在处理或未完成".into());
        }
        if !self.done {
            return Some("CloudAgent 任务在完成事件前结束，响应可能不完整".into());
        }
        None
    }

    /// 收尾：与聚合路径完全一致的错误判定。
    fn finish(mut self, model: &str, session_id: &str) -> Result<Value, String> {
        if let Some(error) = self
            .terminal_error
            .clone()
            .filter(|_| !self.successful_image_analysis)
        {
            return Err(error);
        }
        if self.content.is_empty() {
            return match self.image_analysis.clone() {
                Some(analysis) => {
                    self.content = analysis;
                    Ok(self.completion(model, session_id))
                }
                None => Err("CloudAgent 未返回文本内容；任务可能仍在处理或未完成".into()),
            };
        }
        if !self.done {
            return Err("CloudAgent 任务在完成事件前结束，响应可能不完整".into());
        }
        Ok(self.completion(model, session_id))
    }

    fn completion(&self, model: &str, session_id: &str) -> Value {
        let mut message = json!({"role": "assistant", "content": self.content});
        if !self.reasoning.is_empty() {
            message["reasoning_content"] = json!(self.reasoning);
        }
        json!({
            "id": format!("chatcmpl-{session_id}"),
            "object": "chat.completion",
            "created": chrono::Utc::now().timestamp(),
            "model": model,
            "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
            "usage": self.usage,
            "codearts_channel": "cloudagent_attachments",
        })
    }
}

pub fn parse_events(raw: &[u8], model: &str, session_id: &str) -> Result<Value, String> {
    let text = String::from_utf8_lossy(raw).replace("\r\n", "\n");
    let mut acc = TaskAccumulator::default();
    for block in text.split("\n\n") {
        acc.feed(block);
    }
    acc.finish(model, session_id)
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
    fn reads_session_image_mapping_not_upload_attachment_id() {
        let messages = vec![
            json!({"content":"[ImageId: internal-image-id][ImageName: pixel.png][ImagePath: /attachments/pixel.png] This is an image."}),
        ];
        let (id, path) = image_hint(&messages, "pixel.png").unwrap();
        assert_eq!(id, "internal-image-id");
        let retry = attachment_retry_prompt("describe", &[(id, path)]);
        assert!(retry.contains("internal-image-id"));
        assert!(retry.contains("/attachments/pixel.png"));
        assert!(!retry.contains("imageId：20261101"));
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
    fn keeps_model_answer_after_successful_image_tool_and_permission_guard() {
        let raw = b"event: tool_result\ndata: {\"tool\":\"analyzeImage\",\"status\":\"completed\",\"result\":\"Red\"}\n\nevent: tool_result\ndata: {\"tool\":\"tool_call\",\"status\":\"error\",\"error\":\"The user rejected permission to use this specific tool call.\"}\n\nevent: message\ndata: {\"content\":\"Red\"}\n\nevent: done\ndata: {}\n\n";
        let result = parse_events(raw, "kimi-k3.1-pro", "s1").unwrap();
        assert_eq!(result["model"], "kimi-k3.1-pro");
        assert_eq!(result["choices"][0]["message"]["content"], "Red");
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
