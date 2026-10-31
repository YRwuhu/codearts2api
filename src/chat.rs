//! 上游请求执行：账号选择、并发槽位、签名、失败重试、流式转发。
//!
//! 这是三种对外协议（OpenAI Chat Completions / Responses / Anthropic Messages）
//! 共用的底座。适配层只负责把请求体翻译成 OpenAI chat 格式、再把结果翻译回去，
//! 真正的账号路由与并发控制只在这里实现一次。

use std::pin::Pin;
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::concurrency::{self, SlotGuard};
use crate::models::TARGET;
use crate::params;
use crate::web::WebState;

/// 同一账号内，请求排队等槽位的最长时间。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(120);

/// 后台心跳间隔：长时间没有上游数据时给客户端发一个 SSE 注释帧，
/// 防止客户端/中间代理把空闲连接当死连接掐掉。
const KEEPALIVE: Duration = Duration::from_secs(15);

/// 统一错误。适配层负责把它渲染成各自协议的形状。
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "model_not_found", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable", message)
    }

    /// OpenAI 风格错误体（Chat Completions 与 Responses 通用）。
    pub fn to_openai(&self) -> Response {
        (
            self.status,
            Json(json!({"error": {
                "message": self.message,
                "type": self.code,
                "code": self.code,
            }})),
        )
            .into_response()
    }

    /// Anthropic 风格错误体。
    pub fn to_anthropic(&self) -> Response {
        (
            self.status,
            Json(json!({"type": "error", "error": {
                "type": anthropic_error_type(self.status),
                "message": self.message,
            }})),
        )
            .into_response()
    }
}

fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

/// 流式事件：要么是一个 OpenAI `chat.completion.chunk`，要么是结束标记。
pub enum StreamEvent {
    Chunk(Value),
    Done,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>;

pub enum Outcome {
    /// 聚合好的 `chat.completion` 对象
    Json(Value),
    /// 逐块流式输出
    Stream(EventStream),
}

/// 一次上游调用的输入。
pub struct CallRequest {
    /// 已经翻译成 OpenAI chat 格式的请求体
    pub body: Value,
    /// 是否要流式返回
    pub stream: bool,
    /// 会话键（用于钉号与上游 chat_id）
    pub session_key: Option<String>,
}

/// 执行一次聊天请求。
pub async fn execute(state: &WebState, mut req: CallRequest) -> Result<Outcome, ApiError> {
    let pool = state.pool.read().await.clone();
    pool.ensure_fresh().await;

    let model = req
        .body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(pool.first_model().await)
        .ok_or_else(|| ApiError::not_found("没有可用模型"))?;

    if !pool.contains(&model).await {
        return Err(ApiError::not_found(format!(
            "model '{model}' 不在当前账号可用模型列表中"
        )));
    }

    req.body["model"] = json!(model);

    let cap = pool
        .max_tokens_of(&model)
        .await
        .unwrap_or(state.config().max_tokens_cap);

    if let Some(obj) = req.body.as_object_mut() {
        if let Some((orig, capped)) = params::normalize_params(obj, cap) {
            println!("[param] {model}: max_tokens {orig} -> {capped} (上限 {cap})");
        }
    }

    // 上游按会话计并发时用得到 chat_id；同时保证同账号上同一对话能命中
    // prompt cache。它不能让多个请求共用槽位（实测无效），见 concurrency 模块。
    let chat_id = req
        .session_key
        .as_ref()
        .map(|k| concurrency::upstream_chat_id(k));
    if let Some(cid) = chat_id.as_ref() {
        if let Some(obj) = req.body.as_object_mut() {
            obj.insert("chat_id".to_string(), json!(cid));
            obj.insert("prompt_cache_key".to_string(), json!(cid));
        }
    }

    let mut candidates = pool.accounts_for(&model).await;
    if candidates.is_empty() {
        return Err(ApiError::not_found(format!("model '{model}' 无可用账号")));
    }

    // 会话钉号：同一对话优先留在原账号（prompt cache 亲和）。
    if let Some(key) = req.session_key.as_ref() {
        if let Some(pinned) = state.pins.get(key).await {
            if let Some(pos) = candidates.iter().position(|c| c.account_id() == pinned) {
                let c = candidates.remove(pos);
                candidates.insert(0, c);
                state.pins.touch(key).await;
            } else {
                state.pins.release_account(&pinned).await;
            }
        }
    }

    // 多账号时先剔除冷却中的；全在冷却则退回完整列表（排队等）。
    // 多账号时先剔除冷却中 / 该模型正在冷却 / 当日额度用尽的；
    // 全都不可用时退回完整列表（排队等），而不是直接拒绝。
    if candidates.len() > 1 {
        let daily_cap = state.config().daily_token_cap;
        let mut ready = Vec::new();
        for c in &candidates {
            let gate = state.gate(c.account_id(), upstream_concurrency_limit()).await;
            if !gate.available().await {
                continue;
            }
            if gate.model_cooling(&model).await {
                continue;
            }
            if daily_cap > 0 && gate.daily_tokens().await >= daily_cap {
                continue;
            }
            ready.push(c.clone());
        }
        if !ready.is_empty() {
            candidates = ready;
        }
    }

    // 单账号但额度/模型被挡时，明确告诉调用方原因，别静默失败。
    if candidates.len() == 1 {
        let gate = state
            .gate(candidates[0].account_id(), upstream_concurrency_limit())
            .await;
        let daily_cap = state.config().daily_token_cap;
        if daily_cap > 0 && gate.daily_tokens().await >= daily_cap {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                format!("账号 {} 今日 token 已达上限 {daily_cap}", candidates[0].account_id()),
            ));
        }
    }

    // 流式请求优先用有流式槽位的账号；若所有账号的槽位都占满，
    // **降级为上游非流式**再在本地合成 SSE。
    //
    // 这条降级路径是"只有一个对话却触发并发上限"的最终解法：实测上游对
    // `stream=false` 的请求完全不占会话槽（3 个流槽占满时并发 4 个非流式请求
    // 依然全部成功）。代价是首字延迟变成整包返回，收益是永远不会因为
    // 60 秒的会话残留而把用户拒掉。
    let mut use_upstream_stream = req.stream;
    if req.stream {
        let mut with_slot = Vec::new();
        for c in &candidates {
            let gate = state.gate(c.account_id(), upstream_concurrency_limit()).await;
            if gate.free_stream_slots() > 0 {
                with_slot.push(c.clone());
            }
        }
        if with_slot.is_empty() {
            use_upstream_stream = false;
            println!(
                "[pool] {model}: 流式槽位已满，改用非流式上游并本地合成 SSE（不占会话槽）"
            );
        } else if with_slot.len() < candidates.len() {
            // 有槽位的账号排前面
            candidates = with_slot;
        }
    }

    // 形态定下来之后才写 stream 字段，否则会把 stream:true 发给上游、
    // 让本地"降级"形同虚设（这是第一版的 bug）。
    if let Some(obj) = req.body.as_object_mut() {
        if use_upstream_stream {
            obj.insert("stream".to_string(), json!(true));
            obj.entry("stream_options".to_string())
                .or_insert_with(|| json!({"include_usage": true}));
        } else {
            obj.insert("stream".to_string(), json!(false));
            obj.remove("stream_options");
        }
    }

    let started = std::time::Instant::now();
    let mut last: Option<ApiError> = None;

    for (idx, catalog) in candidates.iter().enumerate() {
        let gate = state
            .gate(catalog.account_id(), upstream_concurrency_limit())
            .await;

        // 等槽位。流式走流式池，非流式走独立的普通池（上游不限后者）。
        let guard = match tokio::time::timeout(ACQUIRE_TIMEOUT, gate.acquire(use_upstream_stream))
            .await
        {
            Ok(Ok(g)) => g,
            Ok(Err(e)) => {
                eprintln!("[{}] {model}: {e}", catalog.account_id());
                last = Some(ApiError::unavailable(e));
                continue;
            }
            Err(_) => {
                println!("[{}] {model}: 等待并发槽位超时", catalog.account_id());
                last = Some(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "等待并发槽位超时，请稍后重试",
                ));
                continue;
            }
        };

        match attempt(
            state,
            catalog,
            &model,
            &req.body,
            use_upstream_stream,
            req.stream,
            chat_id.as_deref(),
            guard,
            gate.clone(),
        )
        .await
        {
            Ok(Attempt::Done(outcome)) => {
                catalog.note_success();
                gate.note_success().await;
                if let Some(key) = req.session_key.as_ref() {
                    state.pins.pin(key, catalog.account_id()).await;
                }
                return Ok(outcome);
            }
            Ok(Attempt::Retry {
                error,
                switch_account,
            }) => {
                catalog.note_failure();
                gate.note_failure(&error.message).await;
                // 模型级失败（该账号没有这个模型 / 该模型额度耗尽）只让这一个
                // 模型冷却，不要把整个账号拖下水。
                if error.code == "model_not_found" {
                    gate.cool_model(&model, 300).await;
                }
                state
                    .usage
                    .record(crate::usage::UsageRecord::failed(
                        &model,
                        catalog.account_id(),
                        started.elapsed().as_millis() as i64,
                        format!("{}: {}", error.code, error.message),
                    ))
                    .await;
                if switch_account && idx + 1 < candidates.len() {
                    println!(
                        "[pool] {model}: 账号 {} 不可用（{}），切换下一个",
                        catalog.account_id(),
                        error.code
                    );
                    last = Some(error);
                    continue;
                }
                return Err(error);
            }
            Err(error) => {
                // 请求无法构造（例如序列化失败），换号也无意义
                gate.note_failure(&error.message).await;
                return Err(error);
            }
        }
    }

    Err(last.unwrap_or_else(|| ApiError::unavailable("上游无可用账号")))
}

enum Attempt {
    Done(Outcome),
    Retry {
        error: ApiError,
        switch_account: bool,
    },
}

fn upstream_concurrency_limit() -> usize {
    std::env::var("CODEARTS_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(3)
}

/// 对单个账号发一次请求。
///
/// `upstream_stream` 是**实际打给上游**的形态；`client_stream` 是客户端要的形态。
/// 两者不同意味着我们在「流式槽位占满」时降级成了非流式上游请求，
/// 需要在这里把完整响应重新拆成 SSE 帧。
#[allow(clippy::too_many_arguments)]
async fn attempt(
    state: &WebState,
    catalog: &std::sync::Arc<crate::models::Catalog>,
    model: &str,
    body: &Value,
    upstream_stream: bool,
    client_stream: bool,
    chat_id: Option<&str>,
    guard: SlotGuard,
    gate: std::sync::Arc<crate::concurrency::AccountGate>,
) -> Result<Attempt, ApiError> {
    let account_id = catalog.account_id().to_string();
    let body_bytes = serde_json::to_vec(body)
        .map_err(|e| ApiError::bad_request(format!("序列化请求失败: {e}")))?;

    // 凭证可能在长跑期间过期（STS 24h，实测偶发提前失效），
    // 这里先尝试续期一次再签名，避免把 401 原样丢给客户端。
    if let Err(e) = catalog.ensure_credentials(state).await {
        eprintln!("[{account_id}] 凭证续期失败: {e}");
    }

    let benefit = !catalog.is_agent_channel(model).await;
    let headers = catalog.sign_headers(&body_bytes, benefit, model).await;

    let mut request = state.http.post(TARGET).body(body_bytes);
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            request = request.header(name, value);
        }
    }
    if let Some(cid) = chat_id {
        request = request
            .header("Chat-Id", cid)
            .header("Session-Id", concurrency::upstream_session_id(cid));
    }

    let upstream = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Attempt::Retry {
                error: ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream_network_error",
                    format!("upstream error: {e}"),
                ),
                switch_account: true,
            })
        }
    };

    let status = upstream.status();

    // HTTP 层失败
    if !status.is_success() {
        let detail = upstream.text().await.unwrap_or_default();
        let text = detail.chars().take(2000).collect::<String>();
        let parsed = serde_json::from_str::<Value>(&detail).unwrap_or(Value::Null);
        let code = parsed
            .get("error_code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let msg = parsed
            .get("error_msg")
            .and_then(|v| v.as_str())
            .unwrap_or(&text)
            .to_string();
        println!(
            "[{account_id}] {model}: 上游 HTTP {status} {code} {}",
            &msg[..msg.len().min(200)]
        );
        return Ok(classify_failure(status, &code, &msg));
    }

    // 流式：把 guard 移进流里，直到流结束才归还槽位。
    // 这是修复"只有一个对话却触发并发上限"的关键——旧实现在函数返回时就
    // 释放了许可，而响应体还在慢慢往外发。
    if upstream_stream {
        let content_type = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        // 上游偶尔对 stream=true 回普通 JSON
        if !content_type.contains("text/event-stream") {
            let raw = upstream.bytes().await.unwrap_or_default();
            let mut result = crate::stream::parse_upstream(&raw);
            if let Some(err) = result.get("error") {
                let code = err
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let msg = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                return Ok(classify_failure(
                    StatusCode::OK,
                    &code,
                    &msg,
                ));
            }
            if let Some(choices) = result.get_mut("choices").and_then(|c| c.as_array_mut()) {
                for choice in choices {
                    if choice.get("finish_reason").map(|v| v.is_null()).unwrap_or(true) {
                        choice["finish_reason"] = json!("stop");
                    }
                }
            }
            let usage = result.get("usage").cloned();
            record_usage(
                state,
                catalog,
                model,
                &account_id,
                true,
                usage.as_ref(),
            )
            .await;
            let stream = single_chunk_stream(result);
            return Ok(Attempt::Done(Outcome::Stream(stream)));
        }

        let model_owned = model.to_string();
        let account_owned = account_id.clone();
        let stream = sse_stream(upstream, guard, gate, state.clone(), model_owned, account_owned);
        return Ok(Attempt::Done(Outcome::Stream(stream)));
    }

    // 非流式
    let raw = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return Ok(Attempt::Retry {
                error: ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "upstream_network_error",
                    format!("读取上游响应失败: {e}"),
                ),
                switch_account: true,
            })
        }
    };
    let mut result = crate::stream::parse_upstream(&raw);

    if let Some(err) = result.get("error") {
        let code = err
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(classify_failure(StatusCode::OK, &code, &msg));
    }

    // 补全 id/created，部分客户端依赖它们
    if result.get("id").map(|v| v.is_null()).unwrap_or(true) {
        result["id"] = json!(format!("chatcmpl-{}", uuid_ish()));
    }
    if result.get("object").map(|v| v.is_null()).unwrap_or(true) {
        result["object"] = json!("chat.completion");
    }
    if result.get("created").map(|v| v.is_null()).unwrap_or(true) {
        result["created"] = json!(chrono::Utc::now().timestamp());
    }

    record_usage(
        state,
        catalog,
        model,
        &account_id,
        client_stream,
        result.get("usage"),
    )
    .await;

    // 客户端要流式、但上游走的是非流式（槽位降级）：
    // 把完整结果拆成 SSE 帧返回，客户端体验不变。
    if client_stream {
        let chunk = chat_chunk_from_completion(&result);
        return Ok(Attempt::Done(Outcome::Stream(paced_stream(
            vec![chunk, result],
        ))));
    }

    Ok(Attempt::Done(Outcome::Json(result)))
}

/// 记录一条用量，并把 token 计入该账号的当日预算。
async fn record_usage(
    state: &WebState,
    catalog: &std::sync::Arc<crate::models::Catalog>,
    model: &str,
    account_id: &str,
    stream: bool,
    usage: Option<&Value>,
) {
    state
        .usage
        .record(crate::usage::UsageRecord::from_upstream(
            model, account_id, 0, stream, usage,
        ))
        .await;

    if let Some(total) = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_i64())
    {
        let gate = state
            .gate(account_id, upstream_concurrency_limit())
            .await;
        gate.add_daily_tokens(total).await;
    }
    let _ = catalog;
}

/// 把「首帧 chunk + 完整对象带 usage」组成一个两帧流。
///
/// 客户端拿到的是标准的流：一帧带正文的 `chat.completion.chunk`，
/// 一帧只带 `usage` 的空 choices chunk（OpenAI 的 include_usage 约定）。
fn paced_stream(mut frames: Vec<Value>) -> EventStream {
    let completion = frames.pop();
    let mut items: Vec<Result<StreamEvent, ApiError>> = Vec::new();
    for f in frames {
        items.push(Ok(StreamEvent::Chunk(f)));
    }
    if let Some(done) = completion {
        // 抬出 usage 单独发一帧，与真流式的形态保持一致
        if let Some(u) = done.get("usage").filter(|u| !u.is_null()) {
            items.push(Ok(StreamEvent::Chunk(json!({
                "id": done.get("id").cloned().unwrap_or(Value::Null),
                "object": "chat.completion.chunk",
                "created": done.get("created").cloned().unwrap_or(Value::Null),
                "model": done.get("model").cloned().unwrap_or(Value::Null),
                "choices": [],
                "usage": u,
            }))));
        }
    }
    items.push(Ok(StreamEvent::Done));
    Box::pin(futures_util::stream::iter(items))
}

/// 把上游错误归类：可换号的账号级错误 vs 客户端错误。
fn classify_failure(status: StatusCode, code: &str, msg: &str) -> Attempt {
    let text = format!("{code} {msg}");

    // max_tokens 超限：告诉客户端真实上限，别只说 "param invalid"
    if msg.contains("max_tokens should be between") {
        return Attempt::Retry {
            error: ApiError::bad_request(format!(
                "上游拒绝该参数（{code}）：{msg}。本代理支持的上限是 {}",
                params::UPSTREAM_MAX_OUTPUT_TOKENS
            )),
            switch_account: false,
        };
    }

    if concurrency::is_concurrency_error(&text) {
        return Attempt::Retry {
            error: ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                format!("上游并发会话数已达上限：{msg}"),
            ),
            switch_account: true,
        };
    }

    if concurrency::is_auth_error(&text) {
        return Attempt::Retry {
            error: ApiError::new(
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                format!("上游鉴权失败：{msg}"),
            ),
            switch_account: true,
        };
    }

    // 该账号没有这个模型
    if code.contains("4004")
        || msg.contains("benefit not found")
        || msg.contains("not registered")
        || msg.contains("model is not")
    {
        return Attempt::Retry {
            error: ApiError::not_found(format!("该账号无此模型：{msg}")),
            switch_account: true,
        };
    }

    // 其余按状态码透传（4xx 是客户端问题，不该换号重试）
    let mapped = match status.as_u16() {
        400 | 200 => StatusCode::BAD_REQUEST,
        401 => StatusCode::UNAUTHORIZED,
        403 => StatusCode::FORBIDDEN,
        404 => StatusCode::NOT_FOUND,
        429 => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    };
    Attempt::Retry {
        error: ApiError::new(mapped, code.to_string(), msg.to_string()),
        switch_account: mapped.is_server_error() || mapped == StatusCode::TOO_MANY_REQUESTS,
    }
}

/// 上游对 stream=true 回了普通 JSON 时，包成一次性的流。
fn single_chunk_stream(result: Value) -> EventStream {
    let chunk = chat_chunk_from_completion(&result);
    Box::pin(futures_util::stream::iter(vec![
        Ok(StreamEvent::Chunk(chunk)),
        Ok(StreamEvent::Done),
    ]))
}

/// 把 `chat.completion` 转成单帧 `chat.completion.chunk`。
pub fn chat_chunk_from_completion(result: &Value) -> Value {
    let mut choices = Vec::new();
    if let Some(list) = result.get("choices").and_then(|c| c.as_array()) {
        for choice in list {
            let mut delta = choice
                .get("message")
                .cloned()
                .unwrap_or_else(|| json!({"role": "assistant", "content": ""}));
            if let Some(obj) = delta.as_object_mut() {
                obj.remove("role");
                obj.insert("role".to_string(), json!("assistant"));
            }
            choices.push(json!({
                "index": choice.get("index").cloned().unwrap_or(json!(0)),
                "delta": delta,
                "finish_reason": choice.get("finish_reason").cloned().unwrap_or(json!("stop")),
            }));
        }
    }
    let mut chunk = json!({
        "id": result.get("id").cloned().unwrap_or(json!(format!("chatcmpl-{}", uuid_ish()))),
        "object": "chat.completion.chunk",
        "created": result.get("created").cloned().unwrap_or(json!(chrono::Utc::now().timestamp())),
        "model": result.get("model").cloned().unwrap_or(Value::Null),
        "choices": choices,
    });
    if let Some(u) = result.get("usage") {
        if !u.is_null() {
            chunk["usage"] = u.clone();
        }
    }
    chunk
}

/// 把上游 SSE 逐行转成 `StreamEvent`，并在结束时归还槽位。
fn sse_stream(
    upstream: reqwest::Response,
    guard: SlotGuard,
    gate: std::sync::Arc<crate::concurrency::AccountGate>,
    state: WebState,
    model: String,
    account: String,
) -> EventStream {
    let byte_stream = upstream.bytes_stream();

    Box::pin(async_stream::stream! {
        // guard 必须在整个流的生命周期内持有；drop 时触发 60s 冷却后归还槽位。
        let _guard = guard;
        let mut upstream_stream = byte_stream;
        let mut buffer: Vec<u8> = Vec::new();
        let mut saw_done = false;
        let mut last_usage: Option<Value> = None;

        let mut ticker = tokio::time::interval(KEEPALIVE);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // 第一次立即触发，跳过

        loop {
            tokio::select! {
                chunk = upstream_stream.next() => {
                    let Some(chunk) = chunk else { break };
                    let Ok(chunk) = chunk else { break };
                    buffer.extend_from_slice(&chunk);

                    while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=pos).collect();
                        let line = &line[..line.len().saturating_sub(1)];

                        match crate::stream::forward_stream_line(line) {
                            crate::stream::StreamAction::Skip => {}
                            crate::stream::StreamAction::Done => {
                                saw_done = true;
                                // 跳出双层循环，统一走下面的收尾逻辑（记录用量）
                                break;
                            }
                            crate::stream::StreamAction::Passthrough => {}
                            crate::stream::StreamAction::Chunk(value) => {
                                if let Some(u) = value.get("usage") {
                                    if !u.is_null() {
                                        last_usage = Some(u.clone());
                                    }
                                }
                                yield Ok(StreamEvent::Chunk(value));
                            }
                            crate::stream::StreamAction::Error(value) => {
                                let code = value
                                    .get("error")
                                    .and_then(|e| e.get("code"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let msg = value
                                    .get("error")
                                    .and_then(|e| e.get("message"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                yield Err(ApiError::new(
                                    StatusCode::BAD_GATEWAY,
                                    if code.is_empty() { "upstream_error".to_string() } else { code },
                                    msg,
                                ));
                                return;
                            }
                        }
                    }
                    if saw_done {
                        break;
                    }
                }
                _ = ticker.tick() => {
                    // SSE 注释帧：不产生内容，只保活
                    yield Ok(StreamEvent::Chunk(Value::Null));
                }
            }
        }

        // 无论正常结束还是上游断流，都补一个 [DONE]，
        // 否则客户端会以为响应被截断。
        yield Ok(StreamEvent::Done);

        state.usage.record(crate::usage::UsageRecord::from_upstream(
            &model,
            &account,
            0,
            true,
            last_usage.as_ref(),
        )).await;
        if let Some(total) = last_usage
            .as_ref()
            .and_then(|u| u.get("total_tokens"))
            .and_then(|v| v.as_i64())
        {
            gate.add_daily_tokens(total).await;
        }
    })
}

/// 构造 SSE 响应体（OpenAI 风格 `data: {...}\n\n`）。
pub fn openai_sse_body(mut stream: EventStream) -> Response {
    let body = async_stream::stream! {
        while let Some(item) = stream.next().await {
            match item {
                Ok(StreamEvent::Chunk(Value::Null)) => {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b": keepalive\n\n"));
                }
                Ok(StreamEvent::Chunk(value)) => {
                    let payload = serde_json::to_string(&value).unwrap_or_default();
                    yield Ok(Bytes::from(format!("data: {payload}\n\n")));
                }
                Ok(StreamEvent::Done) => {
                    yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                    return;
                }
                Err(e) => {
                    let payload = serde_json::to_string(&json!({"error": {
                        "message": e.message,
                        "type": e.code,
                        "code": e.code,
                    }}))
                    .unwrap_or_default();
                    yield Ok(Bytes::from(format!("data: {payload}\n\ndata: [DONE]\n\n")));
                    return;
                }
            }
        }
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
    };
    sse_response(axum::body::Body::from_stream(body))
}

pub fn sse_response(body: axum::body::Body) -> Response {
    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    resp
}

/// 从请求头里提取会话提示。
pub fn header_hint(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-codearts-chat-id")
        .or_else(|| headers.get("x-conversation-id"))
        .or_else(|| headers.get("x-session-id"))
        .and_then(|v| v.to_str().ok())
}

/// 短随机 id（不引入额外依赖）。
pub fn uuid_ish() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 12] = rng.gen();
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_completion_to_chunk() {
        let completion = json!({
            "id": "as-1",
            "created": 100,
            "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        });
        let chunk = chat_chunk_from_completion(&completion);
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["choices"][0]["delta"]["content"], "hi");
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
        assert_eq!(chunk["usage"]["total_tokens"], 3);
    }
}
