//! HTTP 服务：/v1/models 与 /v1/chat/completions。

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

use crate::models::{Catalog, TARGET};
use crate::params;

#[derive(Clone)]
pub struct AppState {
    pub catalog: Catalog,
    pub http: reqwest::Client,
    pub max_tokens_cap: i64,
}

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
        .with_state(state)
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn list_models(State(state): State<AppState>) -> Response {
    state.catalog.ensure_fresh(false).await;
    let data: Vec<Value> = state
        .catalog
        .models()
        .await
        .iter()
        .map(|m| m.to_openai())
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn get_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Response {
    state.catalog.ensure_fresh(false).await;
    if !state.catalog.contains(&model_id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {
                "message": format!("model '{model_id}' not found"),
                "type": "invalid_request_error",
                "code": "model_not_found"
            }})),
        )
            .into_response();
    }
    let models = state.catalog.models().await;
    match models.iter().find(|m| m.id == model_id) {
        Some(m) => Json(m.to_openai()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "invalid_request_error"}})),
    )
        .into_response()
}

async fn chat_completions(
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty body");
    }
    let mut req_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid JSON body"),
    };

    state.catalog.ensure_fresh(false).await;

    let model = req_json
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(state.catalog.first_model().await)
        .unwrap_or_default();

    if !state.catalog.contains(&model).await {
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

    let cap = state
        .catalog
        .max_tokens_of(&model)
        .await
        .unwrap_or(state.max_tokens_cap);

    req_json["model"] = json!(model);
    if let Some(obj) = req_json.as_object_mut() {
        if let Some((orig, capped)) = params::normalize_params(obj, cap) {
            println!("[param] {model}: max_tokens {orig} -> {capped} (上限 {cap})");
        }
    }

    let benefit = !state.catalog.is_agent_channel(&model).await;
    let body_bytes = match serde_json::to_vec(&req_json) {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("serialize error: {e}")),
    };
    let headers = state
        .catalog
        .sign_headers(&body_bytes, benefit, &model)
        .await;

    let mut request = state.http.post(TARGET).body(body_bytes);
    for (k, v) in headers {
        match (
            header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            (Ok(name), Ok(value)) => request = request.header(name, value),
            _ => {}
        }
    }

    let upstream = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            return error_response(StatusCode::BAD_GATEWAY, format!("upstream error: {e}"))
        }
    };

    let status = upstream.status();
    if !status.is_success() {
        let detail = upstream.text().await.unwrap_or_default();
        let text = detail.chars().take(2000).collect::<String>();
        println!("[request] {model}: 上游 HTTP {status} body={}", &text[..text.len().min(500)]);
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": {"message": format!("upstream {status}: {text}")}})),
        )
            .into_response();
    }

    if want_stream {
        return stream_response(state.clone(), upstream, model).await;
    }

    let raw = match upstream.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return error_response(StatusCode::BAD_GATEWAY, format!("read upstream: {e}"))
        }
    };
    let result = crate::stream::parse_upstream(&raw);

    if let Some(err) = result.get("error") {
        let code = err.get("code").cloned().unwrap_or(Value::Null);
        let status = params::error_status(&json!({"error_code": code}));
        println!(
            "[request] {model}: 上游业务错误 {} {}",
            code,
            err.get("message").and_then(|v| v.as_str()).unwrap_or("")
        );
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        return (status, Json(result)).into_response();
    }

    Json(result).into_response()
}

async fn stream_response(
    _state: AppState,
    upstream: reqwest::Response,
    model: String,
) -> Response {
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
                if choice.get("finish_reason").map(|v| v.is_null()).unwrap_or(true) {
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

            // 按行切分，保留不完整的尾部
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

