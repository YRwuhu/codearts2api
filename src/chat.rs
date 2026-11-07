//! 上游请求执行：账号选择、并发槽位、签名、失败重试、流式转发。
//!
//! 这是三种对外协议（OpenAI Chat Completions / Responses / Anthropic Messages）
//! 共用的底座。适配层只负责把请求体翻译成 OpenAI chat 格式、再把结果翻译回去，
//! 真正的账号路由与并发控制只在这里实现一次。

use std::pin::Pin;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::concurrency::{self, SlotGuard};
use crate::models::{Pool, TARGET};
use crate::params;
use crate::web::WebState;

/// 聊天上游的空闲读取上限；不再限制整段流的总时长。
const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(180);

/// 同一账号内，请求排队等槽位的最长时间。
///
/// 需要覆盖一个完整滑动窗口（65 秒）再加一点余量，否则排在队尾的请求会被
/// 自己的超时打断，而它其实马上就能开始。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(180);

/// 上游仍报会话满时的重试次数（窗口边界抖动）。
const SATURATION_RETRIES: usize = 2;

/// 上游内容审核误拦截时的立即重试次数。
///
/// 这类错误（`InferHub.ModelArts.81011.403`）是流内的业务错误帧、token 为 0，
/// 上游根本没有真正处理请求；重新发一次往往就过了。超过次数后照常把错误返回
/// 客户端，不要无限重试烧额度。
const WAF_RETRIES: usize = 5;

/// 流式响应开始转发前的探测窗口。
///
/// WAF 误拦截的 error 帧出现在任何正文之前（实测 2–23 秒）。在这段时间内看到
/// 它才能整单重试；一旦已经往客户端吐过正文增量，重试就会让客户端看到两份
/// 内容，只能把错误如实返回。
const WAF_PROBE_WINDOW: Duration = Duration::from_secs(30);

/// 每次重试前等待多久——足够让窗口里最早的那个流滑出去。
const SATURATION_BACKOFF: Duration = Duration::from_secs(8);

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
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "upstream_unavailable",
            message,
        )
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
///
/// 图片请求走「原生优先」：图片直接发给用户选择的模型；只有上游明确拒绝
/// （406 not multimodal）时才改用 `CODEARTS_VISION_MODEL` 重发一次。
///
/// 注意：流式请求一旦开始输出就不能再回退——此时响应头已经发出、客户端已在
/// 渲染，换模型重发只会让它看到两份内容。所以回退只发生在**尚未输出任何字节**
/// 的失败上；流内 406 直接作为终止错误返回客户端（但不计入账号失败）。
/// CloudAgent 附件会话通道已整体移除。
pub async fn execute(state: &WebState, mut req: CallRequest) -> Result<Outcome, ApiError> {
    // 提示词清洗：把会被上游内容安全网关误判的 ZCode 注入片段改写掉。
    // 三种对外协议进入这里时都已归一化成 chat 格式，所以只接这一处。
    if let Some(fired) = crate::sanitize::apply(&mut req.body) {
        println!("[sanitize] 已改写 WAF 触发片段：{fired}");
    }

    let pool = state.pool.read().await.clone();
    pool.ensure_fresh();

    // Config 是一次请求里要反复读的东西（并发上限、日额度、开关），
    // 每次 state.config() 都会克隆整份配置（含账号列表），这里只取一次。
    let config = state.config();

    let requested_model = req
        .body
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or(pool.first_model().await)
        .ok_or_else(|| ApiError::not_found("没有可用模型"))?;

    if !pool.contains(&requested_model).await {
        return Err(ApiError::not_found(format!(
            "model '{requested_model}' 不在当前账号可用模型列表中"
        )));
    }

    let has_images = match req.body.as_object() {
        Some(body) => params::validate_image_input(body).map_err(ApiError::bad_request)?,
        None => false,
    };

    let mut model = requested_model.clone();
    if has_images && pool.vision_hint(&requested_model).await == Some(false) {
        // 已经确认这个模型不读图：直接回退，省掉一次 406 往返。
        match vision_fallback(&requested_model) {
            VisionFallback::Use(vision) => {
                if !pool.wait_for_model(&vision).await {
                    return Err(vision_unavailable(&requested_model, &vision));
                }
                println!("[vision] {requested_model}: 已确认不支持原生识图，改用 {vision}");
                model = vision;
            }
            other => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "model_not_multimodal",
                    format!(
                        "模型 {requested_model} 不支持图片输入（上游明确拒绝）：{}",
                        other.reason()
                    ),
                ));
            }
        }
    }

    let mut result = run_for_model(state, &config, &pool, &mut req, &model).await;

    // 只有「还没输出任何东西」的 406 才能回退：Outcome::Stream 代表上游已经
    // 接受了请求并开始吐字节，此时换模型会污染客户端看到的流。
    let can_fallback = !matches!(result, Ok(Outcome::Stream(_)));
    if has_images && model == requested_model && can_fallback {
        let rejected = result
            .as_ref()
            .err()
            .filter(|error| error.code == "model_not_multimodal")
            .map(|error| error.message.clone());
        if let Some(message) = rejected {
            pool.note_vision(&requested_model, false).await;
            if let VisionFallback::Use(vision) = vision_fallback(&requested_model) {
                if pool.wait_for_model(&vision).await {
                    println!(
                        "[vision] {requested_model}: 上游拒绝原生识图（{message}），回退 {vision}"
                    );
                    model = vision;
                    result = run_for_model(state, &config, &pool, &mut req, &model).await;
                } else {
                    println!("[vision] {requested_model}: 回退模型 {vision} 尚未同步");
                }
            }
        }
    }

    match result {
        Ok(outcome) => {
            // 只有完整落地的响应才算「这个模型确实能读图」；流式在拿到响应头
            // 时就返回了，那时还没读到任何模型输出，记 true 会让能力缓存偏乐观。
            if has_images && matches!(outcome, Outcome::Json(_)) {
                pool.note_vision(&model, true).await;
            }
            Ok(outcome)
        }
        Err(error) => {
            if has_images && error.code == "model_not_multimodal" {
                pool.note_vision(&model, false).await;
            }
            Err(error)
        }
    }
}

/// 默认的图片回退模型。
///
/// 必须是实测支持原生识图的模型；账号没开通时回退不会生效，会返回原始 406。
pub const DEFAULT_VISION_MODEL: &str = "deepseek-v4.1-flash-1017";

/// `CODEARTS_VISION_MODEL` 的解析结果。
///
/// 分开建模是因为「不启用回退」有三种完全不同的原因，给用户的建议也不同：
/// 关闭、没填、以及配的正好就是当前模型（常见于单模型部署）。
#[derive(Debug, PartialEq, Eq)]
enum VisionFallback {
    Use(String),
    Off,
    Empty,
    SameAsCurrent,
}

impl VisionFallback {
    /// 不可回退时给调用方看的说明。
    fn reason(&self) -> &'static str {
        match self {
            VisionFallback::Use(_) => "已配置回退模型",
            VisionFallback::Off => "CODEARTS_VISION_MODEL=off（已关闭回退）",
            VisionFallback::Empty => "CODEARTS_VISION_MODEL 为空",
            VisionFallback::SameAsCurrent => "CODEARTS_VISION_MODEL 与当前模型相同，回退没有意义",
        }
    }
}

/// 读取回退配置；`off`、空值或与当前模型相同都不启用回退。
fn vision_fallback(current: &str) -> VisionFallback {
    let configured =
        std::env::var("CODEARTS_VISION_MODEL").unwrap_or_else(|_| DEFAULT_VISION_MODEL.to_string());
    classify_vision_fallback(&configured, current)
}

/// `vision_fallback` 的纯函数部分，便于单测。
fn classify_vision_fallback(configured: &str, current: &str) -> VisionFallback {
    let configured = configured.trim();
    if configured.is_empty() {
        return VisionFallback::Empty;
    }
    if configured.eq_ignore_ascii_case("off") {
        return VisionFallback::Off;
    }
    if configured.eq_ignore_ascii_case(current) {
        return VisionFallback::SameAsCurrent;
    }
    VisionFallback::Use(configured.to_string())
}

/// 回退模型在目录里找不到时的错误：说清是回退链路的问题，而不是调用方选错了模型。
fn vision_unavailable(requested: &str, vision: &str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "model_not_multimodal",
        format!(
            "模型 {requested} 不支持图片输入，回退模型 {vision} 尚未同步或当前账号未开通；\
             请刷新模型目录，或把 CODEARTS_VISION_MODEL 改成可用模型"
        ),
    )
}

/// 按指定模型执行一次完整请求（账号选择、并发槽位、签名、失败重试）。
///
/// 现在只处理文本/原生图片；不再有 CloudAgent 附件会话分支。
async fn run_for_model(
    state: &WebState,
    config: &crate::config::Config,
    pool: &Pool,
    req: &mut CallRequest,
    model: &str,
) -> Result<Outcome, ApiError> {
    if !pool.contains(model).await {
        return Err(ApiError::not_found(format!(
            "model '{model}' 不在当前账号可用模型列表中"
        )));
    }

    req.body["model"] = json!(model);

    let cap = pool
        .max_tokens_of(model)
        .await
        .unwrap_or(config.max_tokens_cap);

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

    let mut candidates = pool.accounts_for(model).await;
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
        let daily_cap = config.daily_token_cap;
        let mut ready = Vec::new();
        for c in &candidates {
            let gate = state.gate(c.account_id(), config.concurrency).await;
            if !gate.available().await {
                continue;
            }
            if gate.model_cooling(model).await {
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
            .gate(candidates[0].account_id(), config.concurrency)
            .await;
        let daily_cap = config.daily_token_cap;
        if daily_cap > 0 && gate.daily_tokens().await >= daily_cap {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                format!(
                    "账号 {} 今日 token 已达上限 {daily_cap}",
                    candidates[0].account_id()
                ),
            ));
        }
    }

    // ── 决定上游请求形态 ──────────────────────────────────
    //
    // 上游的 3 会话限制已经由账号级 `x-ot-session-id` 解决（见
    // concurrency 模块），所以**默认全程保持真流式**，不降级。
    //
    // 流式请求唯一会走的非流式路径有三种，都是显式开关：
    //
    // - `CODEARTS_TOOL_TURN_PLAIN=1`：工具轮改非流式，省流式请求数。
    //   代价是工具调用的增量帧丢失（客户端只能一次性拿到完整 tool_calls）。
    // - `CODEARTS_STREAM_DEGRADE=1`：本地并发位占满时降级。
    //   代价同上，外加首字延迟变成整包返回。
    // - 两者都不开时：排队等本地并发位（最多 ACQUIRE_TIMEOUT）。
    //
    // 没有开关时排队而不是降级，是因为降级破坏 agent 类客户端的
    // 边生成边渲染/边执行体验，而这正是"工具调用和编辑文件很怪"的根源。
    let mut use_upstream_stream = req.stream;
    let tool_turn = req.stream && tool_turn(&req.body);
    if tool_turn && config.tool_turn_plain {
        use_upstream_stream = false;
    }

    if req.stream {
        let mut with_slot = Vec::new();
        for c in &candidates {
            let gate = state.gate(c.account_id(), config.concurrency).await;
            if gate.free_stream_slots().await > 0 {
                with_slot.push(c.clone());
            }
        }
        if !with_slot.is_empty() && with_slot.len() < candidates.len() {
            // 有槽位的账号排前面
            candidates = with_slot;
        } else if with_slot.is_empty() && use_upstream_stream {
            if config.stream_degrade {
                use_upstream_stream = false;
                println!("[pool] {model}: 流式名额已满，改用非流式上游并本地合成 SSE（不占名额）");
            } else {
                println!(
                    "[pool] {model}: 流式名额已满，排队等待（如需自动降级设 CODEARTS_STREAM_DEGRADE=1）"
                );
            }
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
    // 内容审核误拦截的重试次数：跨账号累计，但始终原号重试（换号无意义）。
    let mut waf_retries = 0usize;

    for (idx, catalog) in candidates.iter().enumerate() {
        let gate = state.gate(catalog.account_id(), config.concurrency).await;

        // 撞上 TM.00001041 时的重试次数。窗口是按本地时钟估算的，两个请求
        // 同时越过边界时仍可能有一个被上游拒掉；重试一次几乎总能成功，
        // 比把 429 甩给客户端好得多。
        let mut saturation_retries = 0usize;

        loop {
            // 等槽位。流式走滑动窗口 + 并发池，非流式只占本地并发池。
            let guard = match tokio::time::timeout(
                ACQUIRE_TIMEOUT,
                gate.acquire(use_upstream_stream),
            )
            .await
            {
                Ok(Ok(g)) => g,
                Ok(Err(e)) => {
                    eprintln!("[{}] {model}: {e}", catalog.account_id());
                    last = Some(ApiError::unavailable(e));
                    break;
                }
                Err(_) => {
                    println!("[{}] {model}: 等待并发槽位超时", catalog.account_id());
                    last = Some(ApiError::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        "等待并发槽位超时，请稍后重试",
                    ));
                    break;
                }
            };

            let outcome = attempt(
                state,
                catalog,
                model,
                &req.body,
                use_upstream_stream,
                req.stream,
                chat_id.as_deref(),
                started,
                guard,
                gate.clone(),
            )
            .await;

            match outcome {
                Ok(Attempt::Done(outcome)) => {
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
                    // 内容审核误拦截：请求根本没到模型（token=0），原样立即重试。
                    // 不记账号失败、不写失败用量——真正失败时才记一次。
                    if error.code == "waf_blocked" {
                        if waf_retries < WAF_RETRIES {
                            waf_retries += 1;
                            println!(
                                "[{}] {model}: 上游内容审核误拦截，立即重试 ({}/{WAF_RETRIES})",
                                catalog.account_id(),
                                waf_retries
                            );
                            continue;
                        }
                        println!(
                            "[{}] {model}: 内容审核误拦截重试 {WAF_RETRIES} 次仍失败，返回错误",
                            catalog.account_id()
                        );
                    }

                    // 模型能力错误不是账号故障：不计失败、不触发账号冷却。
                    if is_account_failure(&error.code) {
                        gate.note_failure(&error.message).await;
                    }
                    // 模型级失败（该账号没有这个模型 / 该模型额度耗尽）只让这一个
                    // 模型冷却，不要把整个账号拖下水。
                    if error.code == "model_not_found" {
                        gate.cool_model(model, 300).await;
                    }

                    // 上游说会话满了：本地窗口边界抖动，等一个窗口再试一次。
                    // 单账号时这是唯一能做的，别直接拒绝用户。
                    if concurrency::is_concurrency_error(&error.message)
                        && saturation_retries < SATURATION_RETRIES
                        && (!switch_account || idx + 1 >= candidates.len())
                    {
                        saturation_retries += 1;
                        println!(
                            "[{}] {model}: 上游会话仍占满，{} 秒后重试 ({saturation_retries}/{SATURATION_RETRIES})",
                            catalog.account_id(),
                            SATURATION_BACKOFF.as_secs()
                        );
                        tokio::time::sleep(SATURATION_BACKOFF).await;
                        continue;
                    }

                    state
                        .usage
                        .record(crate::usage::UsageRecord::failed(
                            model,
                            catalog.account_id(),
                            started.elapsed().as_millis() as i64,
                            req.stream,
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
                        break;
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
    started: Instant,
    guard: SlotGuard,
    gate: std::sync::Arc<crate::concurrency::AccountGate>,
) -> Result<Attempt, ApiError> {
    let account_id = catalog.account_id().to_string();
    if std::env::var("CODEARTS_DEBUG_REQUEST_SHAPE")
        .ok()
        .as_deref()
        == Some("1")
    {
        println!(
            "[request-shape] {model} body_bytes={} chat_headers={} shape={}",
            serde_json::to_vec(body).map(|b| b.len()).unwrap_or(0),
            chat_id.is_some(),
            params::request_shape(body.as_object().unwrap_or(&serde_json::Map::new()))
        );
    }
    let body_bytes = serde_json::to_vec(body)
        .map_err(|e| ApiError::bad_request(format!("序列化请求失败: {e}")))?;

    // 凭证可能在长跑期间过期（STS 24h，实测偶发提前失效），
    // 这里先尝试续期一次再签名，避免把 401 原样丢给客户端。
    if let Err(e) = catalog.ensure_credentials(state).await {
        eprintln!("[{account_id}] 凭证续期失败: {e}");
    }

    let benefit = !catalog.is_agent_channel(model).await;
    let headers = catalog.sign_headers(&body_bytes, benefit, model).await;

    let mut request = state
        .http
        .post(TARGET)
        .timeout(UPSTREAM_IDLE_TIMEOUT)
        .body(body_bytes);
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
            msg.chars().take(200).collect::<String>()
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
                return Ok(classify_failure(StatusCode::OK, &code, &msg));
            }
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
            let usage = result.get("usage").cloned();
            record_usage(
                state,
                &gate,
                model,
                &account_id,
                true,
                started,
                usage.as_ref(),
            )
            .await;
            // 与「客户端要流式、上游走非流式」那条路径保持同一形态，
            // 顺带把 usage 单独补一帧（旧的单帧包装会把它丢掉）。
            let chunk = chat_chunk_from_completion(&result);
            return Ok(Attempt::Done(Outcome::Stream(paced_stream(vec![
                chunk, result,
            ]))));
        }

        let mut decoder = decoder_for(upstream);
        match probe_stream(&mut decoder).await {
            ProbeOutcome::Waf(error) => {
                // 还没向客户端输出任何正文，整单重试是安全的。
                return Ok(Attempt::Retry {
                    error,
                    switch_account: false,
                });
            }
            ProbeOutcome::Ready { prefix, completion } => {
                let stream = sse_stream(
                    decoder,
                    prefix,
                    completion,
                    guard,
                    gate,
                    state.clone(),
                    model.to_string(),
                    account_id.clone(),
                    started,
                );
                return Ok(Attempt::Done(Outcome::Stream(stream)));
            }
        }
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
        &gate,
        model,
        &account_id,
        client_stream,
        started,
        result.get("usage"),
    )
    .await;

    // 客户端要流式、但上游走的是非流式（槽位降级）：
    // 把完整结果拆成 SSE 帧返回，客户端体验不变。
    if client_stream {
        let chunk = chat_chunk_from_completion(&result);
        return Ok(Attempt::Done(Outcome::Stream(paced_stream(vec![
            chunk, result,
        ]))));
    }

    Ok(Attempt::Done(Outcome::Json(result)))
}

/// 记录一条用量，并把 token 计入该账号的当日预算。
///
/// 调用方已持有该账号的 `gate`，直接复用，避免再按账号查表并克隆一次配置。
async fn record_usage(
    state: &WebState,
    gate: &crate::concurrency::AccountGate,
    model: &str,
    account_id: &str,
    stream: bool,
    started: Instant,
    usage: Option<&Value>,
) {
    state
        .usage
        .record(crate::usage::UsageRecord::from_upstream(
            model,
            account_id,
            started.elapsed().as_millis() as i64,
            stream,
            usage,
        ))
        .await;

    if let Some(total) = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_i64())
    {
        gate.add_daily_tokens(total).await;
    }
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

/// 这个流式请求是不是「工具轮回合」。
///
/// 判定依据（按可靠性排序）：
///
/// 1. 历史里已经有 `tool` 结果 → 上一轮调了工具，这一轮必然还是工具循环。
/// 2. 客户端带了 `tools` 定义 → 它期待模型可能调用工具。
///
/// 这两类请求产出的是工具调用，不需要正文增量渲染，但会消耗宝贵的流式名额。
fn tool_turn(body: &Value) -> bool {
    let has_tool_result = body
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            msgs.iter()
                .any(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        })
        .unwrap_or(false);
    if has_tool_result {
        return true;
    }
    // 没有工具定义就不可能调用工具，这种情况一定是最终回复，保留真流式
    body.get("tools")
        .and_then(|t| t.as_array())
        .map(|t| !t.is_empty())
        .unwrap_or(false)
}

/// 该错误是否应当计入账号失败。
///
/// `model_not_multimodal` 是上游对**模型能力**的拒绝：换账号、续凭证都改变不了
/// 结果，记进失败只会把健康的账号打成冷却（连续 3 次即触发）。
/// 内容审核误拦截同理——它是请求内容触发的，与账号健康无关；连续几次误拦截
/// 也不该把账号打成冷却。
/// 其余错误（429/401/5xx/网络）仍然按账号故障处理。
fn is_account_failure(code: &str) -> bool {
    code != "model_not_multimodal" && code != "waf_blocked"
}

/// 把上游错误归类：可换号的账号级错误 vs 客户端错误。
fn classify_failure(status: StatusCode, code: &str, msg: &str) -> Attempt {
    let text = format!("{code} {msg}");

    // 上游内容审核误拦截。这类请求 token 为 0、模型根本没跑，原样重发往往
    // 就能过；交由 run_for_model 立即重试，不要记账号失败。
    if is_waf_block(code, msg) {
        return Attempt::Retry {
            error: ApiError::new(StatusCode::FORBIDDEN, "waf_blocked", msg.to_string()),
            switch_account: false,
        };
    }

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

    // 模型本身不支持图片：属于客户端能力问题，换账号是徒劳的，也不能计入
    // 账号失败（否则连续几次带图请求会把账号打冷却）。
    if msg.contains("model is not multimodal") || code.contains("001001020") {
        return Attempt::Retry {
            error: ApiError::new(
                StatusCode::BAD_REQUEST,
                "model_not_multimodal",
                format!("模型不支持图片输入（上游明确拒绝）：{msg}"),
            ),
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
        || msg.contains("model is not registered")
        || msg.contains("model is not supported")
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
        413 => StatusCode::PAYLOAD_TOO_LARGE,
        429 => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    };
    Attempt::Retry {
        error: ApiError::new(mapped, code.to_string(), msg.to_string()),
        switch_account: mapped.is_server_error() || mapped == StatusCode::TOO_MANY_REQUESTS,
    }
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

/// 上游字节流解码结果：终止前完成 usage 与每日额度记账。
struct StreamCompletion {
    usage: Option<Value>,
    result: Result<(), ApiError>,
}

enum DecoderEvent {
    Event(Result<StreamEvent, ApiError>),
    Complete(StreamCompletion),
}

type Decoder = std::pin::Pin<Box<dyn Stream<Item = DecoderEvent> + Send>>;

fn decoder_for(upstream: reqwest::Response) -> Decoder {
    Box::pin(decode_sse(upstream.bytes_stream()))
}

enum ProbeOutcome {
    /// 流内出现内容审核误拦截，且还没吐出任何正文：可以整单重试。
    Waf(ApiError),
    /// 可以开始向客户端转发；`prefix` 是探测期间缓存的事件。
    Ready {
        prefix: Vec<Result<StreamEvent, ApiError>>,
        completion: Option<StreamCompletion>,
    },
}

/// 在上游 SSE 转发前短暂探测，识别内容审核误拦截。
///
/// WAF 错误以流内 error 帧返回，token 为 0，且出现在任何正文之前。只要在
/// **第一个非空增量之前**看到它，就能整单重试而不让客户端看到两份内容；一旦
/// 已经出现正文（content / reasoning / tool_calls），重试就不可行，只能如实
/// 转发错误。
///
/// 若 `WAF_PROBE_WINDOW` 内什么都没等到（慢模型），同样放行——不能为了探测
/// 把首字延迟无限拉长。
async fn probe_stream(decoder: &mut Decoder) -> ProbeOutcome {
    let mut prefix: Vec<Result<StreamEvent, ApiError>> = Vec::new();
    let deadline = tokio::time::Instant::now() + WAF_PROBE_WINDOW;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, decoder.next()).await {
            Err(_) | Ok(None) => break,
            Ok(Some(DecoderEvent::Event(event))) => {
                // 心跳帧没有内容，不占客户端带宽也不影响判定，直接丢弃。
                if matches!(&event, Ok(StreamEvent::Chunk(Value::Null))) {
                    continue;
                }
                let meaningful =
                    !matches!(&event, Ok(StreamEvent::Chunk(v)) if !chunk_has_output(v));
                prefix.push(event);
                if meaningful {
                    break;
                }
            }
            Ok(Some(DecoderEvent::Complete(done))) => {
                return match done.result {
                    Err(error) if is_waf_block(&error.code, &error.message) => ProbeOutcome::Waf(
                        ApiError::new(StatusCode::FORBIDDEN, "waf_blocked", error.message),
                    ),
                    result => ProbeOutcome::Ready {
                        prefix,
                        completion: Some(StreamCompletion {
                            usage: done.usage,
                            result,
                        }),
                    },
                };
            }
        }
    }
    ProbeOutcome::Ready {
        prefix,
        completion: None,
    }
}

/// 这个 chunk 是否已经带上了真正的模型输出。
fn chunk_has_output(value: &Value) -> bool {
    let Some(choices) = value.get("choices").and_then(|c| c.as_array()) else {
        return false;
    };
    choices.iter().any(|choice| {
        let Some(delta) = choice.get("delta") else {
            return false;
        };
        let text = |key: &str| {
            delta
                .get(key)
                .and_then(Value::as_str)
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        };
        text("content")
            || text("reasoning_content")
            || delta
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|calls| !calls.is_empty())
                .unwrap_or(false)
    })
}

/// 上游内容审核误拦截的识别。
///
/// CodeArts 返回 `InferHub.ModelArts.81011.403` + 一段固定拒答文案，token 为 0。
/// 这不是账号或参数问题，原样重试常常就能通过。
fn is_waf_block(code: &str, message: &str) -> bool {
    code.contains("81011") || message.contains("我无法提供关于您提问内容的建议")
}

/// 把上游 SSE 逐行转成 `StreamEvent`，并在结束时归还槽位。
#[allow(clippy::too_many_arguments)]
fn sse_stream(
    mut decoder: Decoder,
    prefix: Vec<Result<StreamEvent, ApiError>>,
    completion: Option<StreamCompletion>,
    guard: SlotGuard,
    gate: std::sync::Arc<crate::concurrency::AccountGate>,
    state: WebState,
    model: String,
    account: String,
    started: Instant,
) -> EventStream {
    Box::pin(async_stream::stream! {
        let _guard = guard;
        for event in prefix {
            yield event;
        }
        let mut completion = completion;
        if completion.is_none() {
            while let Some(item) = decoder.next().await {
                match item {
                    DecoderEvent::Event(event) => yield event,
                    DecoderEvent::Complete(done) => { completion = Some(done); break; }
                }
            }
        }
        let Some(completion) = completion else { return };
        match completion.result {
            Ok(()) => {
                record_usage(&state, &gate, &model, &account, true, started, completion.usage.as_ref()).await;
                yield Ok(StreamEvent::Done);
            }
            Err(error) => {
                state.usage.record(crate::usage::UsageRecord::failed(
                    &model,
                    &account,
                    started.elapsed().as_millis() as i64,
                    true,
                    format!("{}: {}", error.code, error.message),
                )).await;
                // 流内 406 是模型能力问题，不是账号故障；计进失败会让连续几次
                // 带图请求把账号打成冷却。这里与 run_for_model 共用同一判定。
                if is_account_failure(&error.code) {
                    gate.note_failure(&error.message).await;
                }
                yield Err(error);
            }
        }
    })
}

/// 在 yield 终态前完成读取，避免 async-stream 在 Done 处暂停而漏记账。
fn decode_sse<S, E>(bytes: S) -> impl Stream<Item = DecoderEvent> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    use async_stream::stream;
    stream! {
        let mut buffer = Vec::new();
        let mut saw_done = false;
        let mut usage = None;
        let mut failure = None;
        let mut bytes = Box::pin(bytes);
        let mut ticker = tokio::time::interval(KEEPALIVE);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        'read: loop {
            tokio::select! {
                chunk = bytes.next() => {
                    match chunk {
                        Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                        Some(Err(e)) => {
                            failure = Some(ApiError::new(StatusCode::BAD_GATEWAY, "upstream_network_error", format!("读取上游流失败: {e}")));
                            break;
                        }
                        None => break,
                    }
                    while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=pos).collect();
                        match crate::stream::forward_stream_line(&line[..line.len() - 1]) {
                            crate::stream::StreamAction::Skip | crate::stream::StreamAction::Passthrough => {}
                            crate::stream::StreamAction::Done => { saw_done = true; break 'read; }
                            crate::stream::StreamAction::Chunk(value) => {
                                if let Some(u) = value.get("usage").filter(|u| !u.is_null()) {
                                    usage = Some(u.clone());
                                }
                                yield DecoderEvent::Event(Ok(StreamEvent::Chunk(value)));
                            }
                            crate::stream::StreamAction::Error(value) => {
                                failure = Some(business_stream_error(&value));
                                break 'read;
                            }
                        }
                    }
                }
                _ = ticker.tick() => yield DecoderEvent::Event(Ok(StreamEvent::Chunk(Value::Null))),
            }
        }

        if !saw_done && failure.is_none() && !buffer.is_empty() {
            match crate::stream::forward_stream_line(&buffer) {
                crate::stream::StreamAction::Done => saw_done = true,
                crate::stream::StreamAction::Chunk(value) => {
                    if let Some(u) = value.get("usage").filter(|u| !u.is_null()) {
                        usage = Some(u.clone());
                    }
                    yield DecoderEvent::Event(Ok(StreamEvent::Chunk(value)));
                }
                crate::stream::StreamAction::Error(value) => failure = Some(business_stream_error(&value)),
                _ => {}
            }
        }
        if !saw_done && failure.is_none() {
            failure = Some(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "upstream_incomplete_response",
                "上游连接在 [DONE] 前结束，响应可能不完整",
            ));
        }
        yield DecoderEvent::Complete(StreamCompletion { usage, result: failure.map_or(Ok(()), Err) });
    }
}

fn business_stream_error(value: &Value) -> ApiError {
    let error = value.get("error");
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upstream_error");
    let msg = error
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("上游返回业务错误");
    if msg.contains("model is not multimodal") || code.contains("001001020") {
        return ApiError::new(
            StatusCode::BAD_REQUEST,
            "model_not_multimodal",
            format!("模型不支持图片输入（上游明确拒绝）：{msg}"),
        );
    }
    // 流内出现内容审核误拦截：统一成 waf_blocked，既不会把账号计成失败，
    // 也便于上层在多协议错误体里识别。
    if is_waf_block(code, msg) {
        return ApiError::new(StatusCode::FORBIDDEN, "waf_blocked", msg);
    }
    let status = StatusCode::from_u16(
        code.rsplit('.')
            .next()
            .and_then(|suffix| suffix.parse::<u16>().ok())
            .filter(|status| (400..=599).contains(status))
            .unwrap_or(502),
    )
    .unwrap_or(StatusCode::BAD_GATEWAY);
    ApiError::new(status, code, msg)
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
        yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
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
    fn detects_tool_turns() {
        // 有工具结果 → 工具轮
        let with_result = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1"}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ]});
        assert!(tool_turn(&with_result));

        // 带工具定义 → 也是工具轮
        let with_tools = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f"}}]
        });
        assert!(tool_turn(&with_tools));

        // 纯对话 → 不是工具轮，必须保留真流式
        let plain = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert!(!tool_turn(&plain));

        // 空工具数组也不算
        let empty_tools = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": []
        });
        assert!(!tool_turn(&empty_tools));
    }

    #[tokio::test]
    async fn decoder_distinguishes_completion_from_truncation() {
        let complete = decode_sse(futures_util::stream::iter(vec![Ok::<_, String>(
            Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]",
            ),
        )]));
        let events: Vec<_> = complete.collect().await;
        assert!(matches!(
            events.last(),
            Some(DecoderEvent::Complete(StreamCompletion {
                result: Ok(()),
                ..
            }))
        ));

        let truncated = decode_sse(futures_util::stream::iter(vec![Ok::<_, String>(
            Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"),
        )]));
        let events: Vec<_> = truncated.collect().await;
        assert!(
            matches!(events.last(), Some(DecoderEvent::Complete(StreamCompletion { result: Err(e), .. })) if e.code == "upstream_incomplete_response")
        );

        let failed = decode_sse(futures_util::stream::iter(vec![
            Ok::<_, String>(Bytes::from_static(b"data: {\"choices\":[]}\n\n")),
            Err("connection reset".to_string()),
        ]));
        let events: Vec<_> = failed.collect().await;
        assert!(
            matches!(events.last(), Some(DecoderEvent::Complete(StreamCompletion { result: Err(e), .. })) if e.code == "upstream_network_error")
        );
    }

    #[test]
    fn classifies_multimodal_and_payload_errors_without_failover() {
        let multimodal = classify_failure(
            StatusCode::OK,
            "InferHub.001001005.400",
            "The request model is not multimodal",
        );
        let Attempt::Retry {
            error,
            switch_account,
        } = multimodal
        else {
            panic!("expected classified error");
        };
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.code, "model_not_multimodal");
        assert!(!switch_account);

        // 上游带业务码的 406 同样归类，不能当成账号故障换号。
        let by_code = classify_failure(
            StatusCode::NOT_ACCEPTABLE,
            "InferHub.001001020.406",
            "The request model is not multimodal",
        );
        let Attempt::Retry {
            error,
            switch_account,
        } = by_code
        else {
            panic!("expected classified error");
        };
        assert_eq!(error.code, "model_not_multimodal");
        assert!(!switch_account);

        let large = classify_failure(
            StatusCode::PAYLOAD_TOO_LARGE,
            "",
            "request entity too large",
        );
        let Attempt::Retry {
            error,
            switch_account,
        } = large
        else {
            panic!("expected classified error");
        };
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!switch_account);

        let stream = business_stream_error(&json!({"error": {
            "code": "InferHub.001001005.400", "message": "The request model is not multimodal"
        }}));
        assert_eq!(stream.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn multimodal_rejection_is_not_an_account_failure() {
        // 模型能力错误换号无用，不能把健康账号打成冷却。
        assert!(!is_account_failure("model_not_multimodal"));
        // 内容审核误拦截与账号健康无关，同样不计失败。
        assert!(!is_account_failure("waf_blocked"));
        for code in [
            "rate_limit_error",
            "authentication_error",
            "upstream_network_error",
            "upstream_unavailable",
        ] {
            assert!(is_account_failure(code), "{code} 应计入账号失败");
        }
    }

    #[test]
    fn recognizes_waf_refusal() {
        assert!(is_waf_block(
            "InferHub.ModelArts.81011.403",
            "抱歉，我无法提供关于您提问内容的建议。作为 AI 编程助手，我更擅长协助您处理代码编写"
        ));
        // 只有文案、码被归一化过时也要能认出来。
        assert!(is_waf_block("", "抱歉，我无法提供关于您提问内容的建议"));
        assert!(!is_waf_block(
            "invalid_request_error",
            "max_tokens too large"
        ));

        let classified = classify_failure(
            StatusCode::FORBIDDEN,
            "InferHub.ModelArts.81011.403",
            "抱歉，我无法提供关于您提问内容的建议",
        );
        let Attempt::Retry {
            error,
            switch_account,
        } = classified
        else {
            panic!("expected classified error");
        };
        assert_eq!(error.code, "waf_blocked");
        assert!(!switch_account, "内容审核误拦截不该换号");

        let stream = business_stream_error(&json!({"error": {
            "code": "InferHub.ModelArts.81011.403",
            "message": "抱歉，我无法提供关于您提问内容的建议"
        }}));
        assert_eq!(stream.code, "waf_blocked");
    }

    #[test]
    fn detects_first_real_output_in_chunks() {
        // 只有 role 的空增量不算输出
        assert!(!chunk_has_output(
            &json!({"choices": [{"delta": {"content": "", "role": "assistant"}}]})
        ));
        assert!(chunk_has_output(
            &json!({"choices": [{"delta": {"content": "你"}}]})
        ));
        assert!(chunk_has_output(
            &json!({"choices": [{"delta": {"reasoning_content": "想"}}]})
        ));
        assert!(chunk_has_output(&json!({"choices": [{"delta": {
            "tool_calls": [{"index": 0, "function": {"name": "Bash"}}]
        }}]})));
        assert!(!chunk_has_output(&json!({"choices": []})));
        assert!(!chunk_has_output(&json!(null)));
    }

    #[tokio::test]
    async fn probe_retries_only_before_any_output() {
        use futures_util::stream::iter;

        // 错误帧出现在正文之前 → 可以整单重试
        let waf = || StreamCompletion {
            usage: None,
            result: Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "InferHub.ModelArts.81011.403",
                "抱歉，我无法提供关于您提问内容的建议",
            )),
        };
        let mut decoder: Decoder = Box::pin(iter(vec![
            DecoderEvent::Event(Ok(StreamEvent::Chunk(
                json!({"choices": [{"delta": {"content": "", "role": "assistant"}}]}),
            ))),
            DecoderEvent::Complete(waf()),
        ]));
        assert!(matches!(
            probe_stream(&mut decoder).await,
            ProbeOutcome::Waf(error) if error.code == "waf_blocked"
        ));

        // 已经出现正文 → 不能再重试；后续错误由转发流原样抛出
        let mut decoder: Decoder = Box::pin(iter(vec![
            DecoderEvent::Event(Ok(StreamEvent::Chunk(
                json!({"choices": [{"delta": {"content": "你好"}}]}),
            ))),
            DecoderEvent::Complete(waf()),
        ]));
        match probe_stream(&mut decoder).await {
            ProbeOutcome::Ready { prefix, completion } => {
                assert_eq!(prefix.len(), 1);
                assert!(completion.is_none(), "未读到的错误留给后续流处理");
            }
            ProbeOutcome::Waf(_) => panic!("已经输出正文后不应重试"),
        }

        // 正常流：第一个正文增量到达就放行
        let mut decoder: Decoder = Box::pin(iter(vec![DecoderEvent::Event(Ok(
            StreamEvent::Chunk(json!({"choices": [{"delta": {"content": "hi"}}]})),
        ))]));
        match probe_stream(&mut decoder).await {
            ProbeOutcome::Ready { prefix, .. } => assert_eq!(prefix.len(), 1),
            ProbeOutcome::Waf(_) => panic!("正常响应不应判成 WAF"),
        }
    }

    #[test]
    fn classifies_vision_fallback_reasons() {
        assert_eq!(
            classify_vision_fallback("", "glm-5.3"),
            VisionFallback::Empty
        );
        assert_eq!(
            classify_vision_fallback("  OFF  ", "glm-5.3"),
            VisionFallback::Off
        );
        // 配的正好是当前模型：不能报成 off。
        assert_eq!(
            classify_vision_fallback("GLM-5.3", "glm-5.3"),
            VisionFallback::SameAsCurrent
        );
        assert_eq!(
            classify_vision_fallback(" deepseek-v4.1-flash-1017 ", "glm-5.3"),
            VisionFallback::Use("deepseek-v4.1-flash-1017".to_string())
        );
        // 三种「不启用」的文案必须能区分开。
        assert_ne!(
            VisionFallback::SameAsCurrent.reason(),
            VisionFallback::Off.reason()
        );
        assert_ne!(VisionFallback::Empty.reason(), VisionFallback::Off.reason());
    }

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
