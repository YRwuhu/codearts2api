//! HTTP 服务：/v1/models 与 /v1/chat/completions。
//!
//! 多账号下同一个模型可能在多个账号可用。请求按账号配置顺序尝试，
//! 遇到"该账号不可用"的错误（并发超限、鉴权失败、模型未授权）才切换，
//! 参数错误等客户端问题直接返回，避免无意义地打满所有账号。

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::models::TARGET;
use crate::params;
use crate::web::WebState;

pub type AppState = WebState;

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/v1/models", get(list_models).options(preflight))
        .route("/v1/models/{*model_id}", get(get_model).options(preflight))
        .route(
            "/v1/chat/completions",
            post(chat_completions).options(preflight),
        )
        .layer(cors)
        .merge(crate::admin::router())
        .merge(crate::web::router())
        .with_state(state)
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn list_models(State(state): State<AppState>) -> Response {
    let pool = state.pool.read().await.clone();
    pool.ensure_fresh().await;
    let data: Vec<Value> = pool
        .merged_models()
        .await
        .iter()
        .map(|m| m.to_openai())
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn get_model(State(state): State<AppState>, Path(model_id): Path<String>) -> Response {
    let pool = state.pool.read().await.clone();
    pool.ensure_fresh().await;
    let models = pool.merged_models().await;
    match models.iter().find(|m| m.id == model_id) {
        Some(m) => Json(m.to_openai()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {
                "message": format!("model '{model_id}' not found"),
                "type": "invalid_request_error",
                "code": "model_not_found"
            }})),
        )
            .into_response(),
    }
}

/// 上游每账号的并发会话上限（实测套餐为 3），可用环境变量覆盖。
fn upstream_concurrency_limit() -> usize {
    std::env::var("CODEARTS_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(3)
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "invalid_request_error"}})),
    )
        .into_response()
}

/// 上游返回的这类错误说明"换个账号可能就好了"，值得切换重试。
fn is_account_switchable(payload: &Value) -> bool {
    let code = payload
        .get("error_code")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_uppercase();
    let msg = payload
        .get("error_msg")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    // 并发会话数已达上限
    if code.contains("TM.00001041") || msg.contains("并发会话数") {
        return true;
    }
    // 鉴权/授权类
    if code.starts_with("APIG.0301") || code.starts_with("APIG.0302") || code.contains("401") {
        return true;
    }
    // 该账号没有这个模型
    if code.contains("4004") || msg.contains("benefit not found") || msg.contains("not registered") {
        return true;
    }
    false
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty body");
    }
    let req_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };

    let pool = state.pool.read().await.clone();
    pool.ensure_fresh().await;

    let model = req_json
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(pool.first_model().await)
        .unwrap_or_default();

    if !pool.contains(&model).await {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {
                "message": format!("model '{model}' 不在当前账号可用模型列表中"),
                "type": "invalid_request_error",
                "param": "model",
                "code": "model_not_found"
            }})),
        )
            .into_response();
    }

    let want_stream = req_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let cap = pool
        .max_tokens_of(&model)
        .await
        .unwrap_or(state.config().max_tokens_cap);

    let mut prepared = req_json.clone();
    prepared["model"] = json!(model);
    if let Some(obj) = prepared.as_object_mut() {
        if let Some((orig, capped)) = params::normalize_params(obj, cap) {
            println!("[param] {model}: max_tokens {orig} -> {capped} (上限 {cap})");
        }
    }

    let started = std::time::Instant::now();
    let mut candidates = pool.accounts_for(&model).await;
    if candidates.is_empty() {
        return error_response(StatusCode::NOT_FOUND, format!("model '{model}' 无可用账号"));
    }

    // 会话钉号：同一对话固定到同一账号，避免上游把每轮都当成新会话占用槽位。
    let header_hint = headers
        .get("x-codearts-chat-id")
        .or_else(|| headers.get("x-conversation-id"))
        .and_then(|v| v.to_str().ok());
    let sess_key = crate::concurrency::session_key(&req_json, header_hint);

    if let Some(key) = sess_key.as_ref() {
        if let Some(pinned) = state.pins.get(key).await {
            // 把钉住的账号排到最前；若它已不可用则释放钉号重新选
            if let Some(pos) = candidates.iter().position(|c| c.account_id() == pinned) {
                let c = candidates.remove(pos);
                candidates.insert(0, c);
                state.pins.touch(key).await;
            } else {
                state.pins.release_account(&pinned).await;
            }
        }
    }

    // 过滤掉冷却中的账号（除非全都在冷却）
    let usable: Vec<_> = {
        let mut v = Vec::new();
        for c in &candidates {
            let gate = state.gate(c.account_id(), upstream_concurrency_limit()).await;
            if gate.available().await {
                v.push(c.clone());
            }
        }
        if v.is_empty() {
            candidates.clone()
        } else {
            v
        }
    };
    let candidates = usable;
    if candidates.is_empty() {
        return error_response(StatusCode::NOT_FOUND, format!("model '{model}' 无可用账号"));
    }

    // 依次尝试各账号；只有"账号级"错误才继续下一个。
    let mut last: Option<Response> = None;
    let mut switched = 0usize;

    for (idx, catalog) in candidates.iter().enumerate() {
        let gate = state.gate(catalog.account_id(), upstream_concurrency_limit()).await;

        // 等待并发槽位（超出上游上限时排队而非直接失败）
        let _permit = match tokio::time::timeout(
            std::time::Duration::from_secs(60),
            gate.acquire(),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                println!("[{}] {model}: 跳过（{e}）", catalog.account_id());
                last = Some(error_response(StatusCode::SERVICE_UNAVAILABLE, e));
                continue;
            }
            Err(_) => {
                println!("[{}] {model}: 等待并发槽位超时", catalog.account_id());
                last = Some(error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "等待并发槽位超时，请稍后重试",
                ));
                continue;
            }
        };

        let benefit = !catalog.is_agent_channel(&model).await;
        let body_bytes = match serde_json::to_vec(&prepared) {
            Ok(v) => v,
            Err(e) => {
                return error_response(StatusCode::BAD_REQUEST, format!("serialize error: {e}"))
            }
        };
        let headers = catalog.sign_headers(&body_bytes, benefit, &model).await;

        let mut request = state.http.post(TARGET).body(body_bytes);
        for (k, v) in headers {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(&v),
            ) {
                request = request.header(name, value);
            }
        }

        let upstream = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                catalog.note_failure();
                println!("[{}] {model}: 请求失败 {e}", catalog.account_id());
                last = Some(error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream error: {e}"),
                ));
                continue;
            }
        };

        let status = upstream.status();
        if !status.is_success() {
            let detail = upstream.text().await.unwrap_or_default();
            let text = detail.chars().take(2000).collect::<String>();
            let parsed = serde_json::from_str::<Value>(&detail).unwrap_or(Value::Null);
            println!(
                "[{}] {model}: 上游 HTTP {status} body={}",
                catalog.account_id(),
                &text[..text.len().min(300)]
            );
            gate.note_failure(&text).await;
            if is_account_switchable(&parsed) && idx + 1 < candidates.len() {
                catalog.note_failure();
                switched += 1;
                continue;
            }
            catalog.note_failure();
            state
                .usage
                .record(crate::usage::UsageRecord::failed(
                    &model,
                    catalog.account_id(),
                    started.elapsed().as_millis() as i64,
                    format!("HTTP {status}"),
                ))
                .await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": {"message": format!("upstream {status}: {text}")}})),
            )
                .into_response();
        }

        if want_stream {
            catalog.note_success();
            gate.note_success().await;
            if let Some(key) = sess_key.as_ref() {
                state.pins.pin(key, catalog.account_id()).await;
            }
            if switched > 0 {
                println!("[pool] {model}: 已切换到账号 {}", catalog.account_id());
            }
            state
                .usage
                .record(crate::usage::UsageRecord::from_upstream(
                    &model,
                    catalog.account_id(),
                    started.elapsed().as_millis() as i64,
                    true,
                    None,
                ))
                .await;
            return stream_response(upstream, model).await;
        }

        let raw = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                catalog.note_failure();
                last = Some(error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("read upstream: {e}"),
                ));
                continue;
            }
        };
        let result = crate::stream::parse_upstream(&raw);

        if let Some(err) = result.get("error") {
            let code = err.get("code").cloned().unwrap_or(Value::Null);
            let err_status = params::error_status(&json!({"error_code": code}));
            println!(
                "[{}] {model}: 上游业务错误 {} {}",
                catalog.account_id(),
                code,
                err.get("message").and_then(|v| v.as_str()).unwrap_or("")
            );

            // 200 里包的业务错误同样可能属于账号级，值得换号重试。
            let switchable = result
                .get("error")
                .map(|e| {
                    is_account_switchable(&json!({
                        "error_code": e.get("code").cloned().unwrap_or(Value::Null),
                        "error_msg": e.get("message").cloned().unwrap_or(Value::Null),
                    }))
                })
                .unwrap_or(false);
            if switchable && idx + 1 < candidates.len() {
                catalog.note_failure();
                switched += 1;
                continue;
            }

            let status = StatusCode::from_u16(err_status).unwrap_or(StatusCode::BAD_GATEWAY);
            return (status, Json(result)).into_response();
        }

        catalog.note_success();
        gate.note_success().await;
        if let Some(key) = sess_key.as_ref() {
            state.pins.pin(key, catalog.account_id()).await;
        }
        if switched > 0 {
            println!("[pool] {model}: 已切换到账号 {}", catalog.account_id());
        }
        state
            .usage
            .record(crate::usage::UsageRecord::from_upstream(
                &model,
                catalog.account_id(),
                started.elapsed().as_millis() as i64,
                false,
                result.get("usage"),
            ))
            .await;
        return Json(result).into_response();
    }

    last.unwrap_or_else(|| error_response(StatusCode::BAD_GATEWAY, "上游无可用账号"))
}

async fn stream_response(upstream: reqwest::Response, model: String) -> Response {
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    // 上游偶尔在 stream=true 时返回普通 JSON，需要包装成单帧 SSE。
    if !content_type.contains("text/event-stream") {
        let raw = upstream.bytes().await.unwrap_or_default();
        let mut result = crate::stream::parse_upstream(&raw);
        if let Some(choices) = result.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                if choice
                    .get("finish_reason")
                    .map(|v| v.is_null())
                    .unwrap_or(true)
                {
                    choice["finish_reason"] = json!("stop");
                }
            }
        }
        let payload = serde_json::to_string(&result).unwrap_or_default();
        let body = format!("data: {payload}\n\ndata: [DONE]\n\n");
        return sse_response(Body::from(body));
    }

    let mut upstream_stream = upstream.bytes_stream();
    let stream = async_stream::stream! {
        let mut saw_done = false;
        let mut buffer: Vec<u8> = Vec::new();

        while let Some(chunk) = upstream_stream.next().await {
            let Ok(chunk) = chunk else { break };
            buffer.extend_from_slice(&chunk);

            while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                let line = &line[..line.len().saturating_sub(1)];

                match crate::stream::forward_stream_line(line) {
                    crate::stream::StreamAction::Skip => {}
                    crate::stream::StreamAction::Done => {
                        saw_done = true;
                        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
                    }
                    crate::stream::StreamAction::Passthrough(text) => {
                        let frame = format!("{text}\n\n");
                        yield Ok(Bytes::from(frame));
                    }
                    crate::stream::StreamAction::Chunk(value) => {
                        let payload = serde_json::to_string(&value).unwrap_or_default();
                        let frame = format!("data: {payload}\n\n");
                        yield Ok(Bytes::from(frame));
                    }
                    crate::stream::StreamAction::Error(value) => {
                        let payload = serde_json::to_string(&value).unwrap_or_default();
                        println!("[stream] {model}: 上游业务错误 {payload}");
                        let frame = format!("data: {payload}\n\ndata: [DONE]\n\n");
                        yield Ok(Bytes::from(frame));
                        return;
                    }
                }
            }
        }

        if !saw_done {
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    };

    sse_response(Body::from_stream(stream))
}

fn sse_response(body: Body) -> Response {
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
