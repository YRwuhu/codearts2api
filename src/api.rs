//! HTTP 路由：OpenAI Chat Completions / OpenAI Responses / Anthropic Messages。
//!
//! 三种协议共用 `chat::execute` 这一条上游通路；本模块只做路由与
//! 「请求格式 → OpenAI chat → 响应格式」的翻译。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::chat::{self, CallRequest, Outcome};
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
        .route("/v1/responses", post(responses).options(preflight))
        .route("/v1/messages", post(anthropic_messages).options(preflight))
        .route(
            "/v1/messages/count_tokens",
            post(anthropic_count_tokens).options(preflight),
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

fn parse_body(body: &Bytes) -> Result<Value, Response> {
    if body.is_empty() {
        return Err(chat::ApiError::bad_request("empty body").to_openai());
    }
    serde_json::from_slice(body)
        .map_err(|e| chat::ApiError::bad_request(format!("invalid JSON body: {e}")).to_openai())
}

// ── OpenAI Chat Completions ──────────────────────────────────

async fn chat_completions(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let req_json = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let stream = req_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let session_key = crate::concurrency::session_key(&req_json, chat::header_hint(&headers));

    let mut prepared = req_json.clone();
    if let Some(obj) = prepared.as_object_mut() {
        // 这几个字段上游不认，且会触发 400。
        for key in ["safety_identifier", "previous_response_id"] {
            obj.remove(key);
        }
    }

    let request = CallRequest {
        body: prepared,
        stream,
        session_key,
    };

    match chat::execute(&state, request).await {
        Ok(Outcome::Json(v)) => Json(v).into_response(),
        Ok(Outcome::Stream(s)) => chat::openai_sse_body(s),
        Err(e) => e.to_openai(),
    }
}

// ── OpenAI Responses API ────────────────────────────────────

async fn responses(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let req_json = match parse_body(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let stream = req_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let session_key = crate::concurrency::session_key(&req_json, chat::header_hint(&headers))
        .or_else(|| {
            req_json
                .get("previous_response_id")
                .and_then(|v| v.as_str())
                .map(|s| format!("resp:{s}"))
        });

    let body = match crate::responses::to_chat_request(&req_json) {
        Ok(v) => v,
        Err(e) => return e.to_openai(),
    };

    let request = CallRequest {
        body,
        stream,
        session_key,
    };

    match chat::execute(&state, request).await {
        Ok(Outcome::Json(v)) => Json(crate::responses::from_chat_completion(&v, &req_json))
            .into_response(),
        Ok(Outcome::Stream(s)) => {
            crate::responses::stream_body(s, req_json.get("model").and_then(|v| v.as_str()))
        }
        Err(e) => e.to_openai(),
    }
}

// ── Anthropic Messages ──────────────────────────────────────

async fn anthropic_messages(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let req_json = match parse_body(&body) {
        Ok(v) => v,
        Err(_) => {
            return chat::ApiError::bad_request("invalid JSON body").to_anthropic();
        }
    };

    let stream = req_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let session_key = crate::concurrency::session_key(&req_json, chat::header_hint(&headers));

    let body = match crate::anthropic::to_chat_request(&req_json) {
        Ok(v) => v,
        Err(e) => return e.to_anthropic(),
    };
    let model = req_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let request = CallRequest {
        body,
        stream,
        session_key,
    };

    match chat::execute(&state, request).await {
        Ok(Outcome::Json(v)) => {
            Json(crate::anthropic::from_chat_completion(&v, &model, &req_json)).into_response()
        }
        Ok(Outcome::Stream(s)) => {
            crate::anthropic::sse_body(s, model, req_json.get("model").cloned())
        }
        Err(e) => e.to_anthropic(),
    }
}

/// Anthropic 客户端在发消息前会先估算 token 数；这里给一个可用的近似值，
/// 而不是让整个请求失败。
async fn anthropic_count_tokens(
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let req_json = match parse_body(&body) {
        Ok(v) => v,
        Err(_) => return chat::ApiError::bad_request("invalid JSON body").to_anthropic(),
    };
    let _ = state;
    let tokens = crate::anthropic::estimate_tokens(&req_json);
    Json(json!({"input_tokens": tokens})).into_response()
}

