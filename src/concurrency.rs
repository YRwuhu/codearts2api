//! 会话槽位管理。
//!
//! # 上游到底怎么限并发（实测，2026-10）
//!
//! 对同一个 CodeArts 账号打 `/api/v2/chat/completions`：
//!
//! | 请求形态 | 首个请求 | 连发 3 个后 | 说明 |
//! |---|---|---|---|
//! | `stream=false` | OK | 全部 OK | 不占会话槽，几乎没有限制 |
//! | `stream=true`  | OK | 第 4 个起 `TM.00001041` | 每个流占 1 个槽，上限 3 |
//!
//! 关键点：**槽位不是随请求结束释放的**。流式请求结束后，会话仍被上游保留
//! 一段时间（实测约 30–60 秒，期间同账号再开新流式会话会继续报
//! `TM.00001041 并发会话数已达上限(3个)`）。`chat_id` / `Chat-Id` /
//! `Session-Id` / `prompt_cache_key` 都**不能**让多个请求复用同一个槽位，
//! 也不能提前释放——同一 chat_id 连发 6 次，照样在第 4 次被拒。
//!
//! 所以「把同一对话钉到同一账号、复用 chat_id」并不能绕过限制。真正能做的
//! 只有两件事：
//!
//! 1. **本地记账**：代理自己数每个账号还有几个槽，用完了就排队 / 换号，
//!    而不是把注定 400 的请求打上去。
//! 2. **让流式请求尽量少**：能在本地把 `stream=false` 的请求聚合好再返回、
//!    把并发压到 1，就不占槽。
//!
//! # 这个模块做什么
//!
//! - [`AccountGate`]：每账号的流式槽位池。槽位在流式响应**真正结束**时归还，
//!   并额外保留一段「冷却」时间（[`SLOT_COOLDOWN`]），因为上游释放得更慢；
//!   冷却期内槽位不算空闲，但会随时间自动回收。
//! - [`SessionPins`]：把同一对话固定到同一账号。这**不是**为了省槽位，
//!   而是为了上游 prompt cache 亲和（同账号同前缀才命中）以及避免同一对话
//!   在两轮之间跳到不同账号导致行为不一致。
//! - [`session_key`]：从请求里稳定地提取对话键。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};

/// 流式会话结束后，上游仍占用槽位的时间。
///
/// 实测：3 个流打满后，新的流式请求在第 58 秒左右才恢复。取 60 秒留足余量；
/// 设成 0 相当于「请求一结束就认为槽位可用」，会持续撞 `TM.00001041`。
pub const SLOT_COOLDOWN: Duration = Duration::from_secs(60);

/// 会话钉号的有效期；超过后重新选号。
const PIN_TTL: Duration = Duration::from_secs(30 * 60);

/// 单个账号的流式并发闸门与健康状态。
pub struct AccountGate {
    /// 上游允许的并发流式会话数（默认 3）
    limit: usize,
    /// 流式槽位
    stream_slots: Arc<Semaphore>,
    /// 非流式并发（上游对这种请求几乎不限，但本地仍限个上限防止打爆）
    plain_slots: Arc<Semaphore>,
    state: Arc<RwLock<GateState>>,
}

#[derive(Default)]
struct GateState {
    /// 当前占用的流式槽位（计入冷却中的）
    stream_in_flight: u32,
    plain_in_flight: u32,
    /// 冷却截止时间（触发并发上限后短暂避让）
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    total_requests: u64,
    total_failures: u64,
    /// 最近一次 `TM.00001041` 的时间，用于自适应退避
    last_saturation: Option<Instant>,
    last_error: Option<String>,
    /// 按模型隔离的冷却（账号级冷却会让整个账号不可用，模型级只挡那一个模型）
    model_cooldowns: HashMap<String, Instant>,
    /// 当日 token 消耗与配额（借鉴 workbuddy2api 的每日上限）
    daily_tokens: i64,
    /// 日切时间戳（本地日期 0 点的秒级时间）
    daily_reset_ts: i64,
}

impl AccountGate {
    pub fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            limit,
            stream_slots: Arc::new(Semaphore::new(limit)),
            // 非流式请求不占上游会话槽，但限制一下并发避免把带宽打满
            plain_slots: Arc::new(Semaphore::new(limit.saturating_mul(4).max(4))),
            state: Arc::new(RwLock::new(GateState::default())),
        }
    }

    /// 该账号现在是否可用（不在冷却中）。
    pub async fn available(&self) -> bool {
        let st = self.state.read().await;
        match st.cooldown_until {
            Some(until) => Instant::now() >= until,
            None => true,
        }
    }

    /// 现在还有几个流式槽位空着（冷却中的不计入空闲）。
    pub fn free_stream_slots(&self) -> usize {
        self.stream_slots.available_permits()
    }

    /// 获取一个槽位。流式请求占用上游会话槽，非流式只占本地并发许可。
    ///
    /// 上游对非流式请求不设会话上限，因此两者分开计数：把非流式请求也算进
    /// 那 3 个槽里，会让「一次对话」在客户端并发轮询时凭空触发限制。
    pub async fn acquire(&self, stream: bool) -> Result<SlotGuard, String> {
        let sem = if stream {
            self.stream_slots.clone()
        } else {
            self.plain_slots.clone()
        };
        let permit = sem
            .acquire_owned()
            .await
            .map_err(|_| "并发闸门已关闭".to_string())?;

        {
            let mut st = self.state.write().await;
            if stream {
                st.stream_in_flight += 1;
            } else {
                st.plain_in_flight += 1;
            }
            st.total_requests += 1;
        }

        Ok(SlotGuard {
            permit: Some(permit),
            stream,
            state: self.state.clone(),
        })
    }

    /// 记录一次成功，清除失败计数。
    pub async fn note_success(&self) {
        let mut st = self.state.write().await;
        st.consecutive_failures = 0;
        st.last_error = None;
    }

    /// 记录一次失败。并发上限类错误触发短暂冷却，避免继续撞墙。
    pub async fn note_failure(&self, error: &str) {
        let mut st = self.state.write().await;
        st.total_failures += 1;
        st.consecutive_failures += 1;
        st.last_error = Some(error.chars().take(200).collect());

        if is_concurrency_error(error) {
            // 并发上限：说明本地记账落后于上游，暂停一段时间让上游先释放
            st.last_saturation = Some(Instant::now());
            st.cooldown_until = Some(Instant::now() + SLOT_COOLDOWN);
        } else if st.consecutive_failures >= 3 {
            // 连续失败：指数避让，最长 5 分钟
            let secs = (10u64 * st.consecutive_failures as u64).min(300);
            st.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    /// 若在冷却中返回剩余秒数。
    #[cfg(test)]
    pub async fn cooldown_secs_if_any(&self) -> Option<u64> {
        let st = self.state.read().await;
        st.cooldown_until.map(|until| {
            let now = Instant::now();
            if now >= until {
                0
            } else {
                (until - now).as_secs() + 1
            }
        })
    }

    /// 该模型的冷却是否仍在生效。
    pub async fn model_cooling(&self, model: &str) -> bool {
        let st = self.state.read().await;
        match st.model_cooldowns.get(model) {
            Some(until) => Instant::now() < *until,
            None => false,
        }
    }

    /// 让某个模型在这个账号上冷却一段时间。
    ///
    /// 与账号级冷却的区别：模型级只挡这一个模型（例如该模型在本账号额度用尽），
    /// 其余模型照常可用。这是 workbuddy2api 的 `model_cooldowns` 结构。
    pub async fn cool_model(&self, model: &str, secs: u64) {
        let mut st = self.state.write().await;
        st.model_cooldowns
            .insert(model.to_string(), Instant::now() + Duration::from_secs(secs));
    }

    /// 累加当日 token 消耗，跨日自动归零。
    pub async fn add_daily_tokens(&self, tokens: i64) -> i64 {
        let mut st = self.state.write().await;
        let today = local_midnight_ts();
        if st.daily_reset_ts != today {
            st.daily_reset_ts = today;
            st.daily_tokens = 0;
            // 顺带清掉过期的模型冷却，避免 Map 无限增长
            let now = Instant::now();
            st.model_cooldowns.retain(|_, until| *until > now);
        }
        st.daily_tokens += tokens.max(0);
        st.daily_tokens
    }

    /// 当日已用 token（跨日视为 0）。
    pub async fn daily_tokens(&self) -> i64 {
        let st = self.state.read().await;
        if st.daily_reset_ts != local_midnight_ts() {
            0
        } else {
            st.daily_tokens
        }
    }

    /// 快照，供 WebUI 展示。
    pub async fn snapshot(&self) -> serde_json::Value {
        let st = self.state.read().await;
        let now = Instant::now();
        serde_json::json!({
            "limit": self.limit,
            "in_flight": st.stream_in_flight,
            "stream_in_flight": st.stream_in_flight,
            "plain_in_flight": st.plain_in_flight,
            "free_stream_slots": self.stream_slots.available_permits(),
            "available": st.cooldown_until.map(|u| now >= u).unwrap_or(true),
            "cooldown_remaining": st.cooldown_until
                .map(|u| if now >= u { 0 } else { (u - now).as_secs() })
                .unwrap_or(0),
            "consecutive_failures": st.consecutive_failures,
            "total_requests": st.total_requests,
            "total_failures": st.total_failures,
            "last_error": st.last_error,
            "daily_tokens": st.daily_tokens,
            "model_cooldowns": st.model_cooldowns.iter()
                .filter(|(_, until)| **until > now)
                .map(|(m, until)| (m.clone(), until.saturating_duration_since(now).as_secs()))
                .collect::<HashMap<String, u64>>(),
        })
    }
}

/// 占用的槽位。流式请求结束（响应体全部发完）后 drop，
/// 并在 [`SLOT_COOLDOWN`] 之后才真正把许可还给闸门。
pub struct SlotGuard {
    permit: Option<OwnedSemaphorePermit>,
    stream: bool,
    state: Arc<RwLock<GateState>>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        // in_flight 需要在异步上下文里减；用 try_write 避免 drop 中 await
        let stream = self.stream;
        if let Ok(mut st) = self.state.try_write() {
            if stream {
                st.stream_in_flight = st.stream_in_flight.saturating_sub(1);
            } else {
                st.plain_in_flight = st.plain_in_flight.saturating_sub(1);
            }
        } else {
            let state = self.state.clone();
            tokio::spawn(async move {
                let mut st = state.write().await;
                if stream {
                    st.stream_in_flight = st.stream_in_flight.saturating_sub(1);
                } else {
                    st.plain_in_flight = st.plain_in_flight.saturating_sub(1);
                }
            });
        }

        let Some(permit) = self.permit.take() else {
            return;
        };
        if !stream {
            // 非流式请求上游不占会话，立即归还
            drop(permit);
            return;
        }
        // 流式请求：上游释放槽位有明显延迟，冷却期过后再归还，
        // 否则本地以为有空位、打上去却拿到 TM.00001041。
        tokio::spawn(async move {
            tokio::time::sleep(SLOT_COOLDOWN).await;
            drop(permit);
        });
    }
}

/// 本地时区当天 0 点的秒级时间戳（用于日额度归零）。
fn local_midnight_ts() -> i64 {
    use chrono::TimeZone;
    let now = chrono::Local::now();
    let date = now.date_naive();
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .unwrap_or_else(|| now.naive_local());
    chrono::Local
        .from_local_datetime(&midnight)
        .single()
        .map(|t| t.timestamp())
        .unwrap_or_else(|| now.timestamp())
}

/// 上游并发上限错误的判定。
pub fn is_concurrency_error(text: &str) -> bool {
    text.contains("TM.00001041") || text.contains("并发会话数")
}

/// 上游鉴权/凭证过期：可切换账号，也可续期。
pub fn is_auth_error(text: &str) -> bool {
    text.contains("APIG.0301")
        || text.contains("APIG.0302")
        || text.contains("APIG.0602")
        || text.contains("security token has expired")
        || text.contains("verify ak sk signature fail")
}

// ── 会话钉号 ────────────────────────────────────────────────

/// 会话到账号的映射，带 TTL。
#[derive(Clone, Default)]
pub struct SessionPins {
    inner: Arc<Mutex<HashMap<String, Pin>>>,
}

struct Pin {
    account_id: String,
    updated_at: Instant,
}

impl SessionPins {
    pub fn new() -> Self {
        Self::default()
    }

    /// 查该会话钉在哪个账号。
    pub async fn get(&self, session_key: &str) -> Option<String> {
        let mut guard = self.inner.lock().await;
        guard.retain(|_, p| p.updated_at.elapsed() < PIN_TTL);
        guard.get(session_key).map(|p| p.account_id.clone())
    }

    /// 钉号（存在则刷新 TTL）。
    pub async fn pin(&self, session_key: &str, account_id: &str) {
        let mut guard = self.inner.lock().await;
        guard.insert(
            session_key.to_string(),
            Pin {
                account_id: account_id.to_string(),
                updated_at: Instant::now(),
            },
        );
    }

    /// 仅刷新 TTL（命中已有钉号时用）。
    pub async fn touch(&self, session_key: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(p) = guard.get_mut(session_key) {
            p.updated_at = Instant::now();
        }
    }

    /// 清除某账号的所有钉号（账号被禁用/删除时调用）。
    pub async fn release_account(&self, account_id: &str) {
        let mut guard = self.inner.lock().await;
        guard.retain(|_, p| p.account_id != account_id);
    }

    pub async fn len(&self) -> usize {
        let guard = self.inner.lock().await;
        guard.len()
    }
}

/// 把本地会话键转成稳定的上游 chat_id。
///
/// 注意：这**不会**让请求复用上游会话槽（实测无效），只为让同账号上的
/// 同一对话命中 prompt cache、并在日志里可追踪。
pub fn upstream_chat_id(session_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"codearts-chat:");
    hasher.update(session_key.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("chat-{}", &digest[..24])
}

/// 由 chat_id 派生稳定的 Session-Id（prompt cache 亲和）。
pub fn upstream_session_id(chat_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"codearts-session:");
    hasher.update(chat_id.as_bytes());
    hex::encode(&hasher.finalize()[..16])
}

/// 从 OpenAI 请求体里提取会话键。
///
/// 优先用客户端显式传入的标识（`user` 字段、`X-Codearts-Chat-Id` 头），
/// 否则退化为「首条 user 消息内容的哈希」——同一对话的续轮通常带相同历史，
/// 哈希一致即可稳定钉号。
///
/// 内容既可能是纯字符串，也可能是多模态数组（`[{"type":"text","text":...}]`），
/// 后者的旧实现会直接返回 None，导致所有多模态对话都拿到同一个空键。
pub fn session_key(body: &serde_json::Value, header_hint: Option<&str>) -> Option<String> {
    if let Some(h) = header_hint {
        let h = h.trim();
        if !h.is_empty() {
            return Some(format!("hdr:{h}"));
        }
    }

    if let Some(user) = body.get("user").and_then(|v| v.as_str()) {
        let user = user.trim();
        if !user.is_empty() {
            return Some(format!("user:{user}"));
        }
    }

    // Codex / 部分客户端把对话标识塞在 metadata 或 prompt_cache_key 里
    for field in ["prompt_cache_key", "conversation_id"] {
        if let Some(v) = body.get(field).and_then(|v| v.as_str()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(format!("{field}:{v}"));
            }
        }
    }

    // 退化：取第一条 user 消息的哈希（同一对话首条消息固定）
    let messages = body.get("messages")?.as_array()?;
    let first_user = messages
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let text = message_text(first_user.get("content")?);
    if text.is_empty() {
        return None;
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hex::encode(hasher.finalize());
    Some(format!("msg:{}", &digest[..16]))
}

/// 把 content 统一成可哈希的文本（兼容字符串与多模态数组）。
pub fn message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| p.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_concurrency_error() {
        assert!(is_concurrency_error("TM.00001041"));
        assert!(is_concurrency_error("并发会话数已达上限(3个)"));
        assert!(!is_concurrency_error("benefit not found"));
    }

    #[test]
    fn detects_auth_error() {
        assert!(is_auth_error("APIG.0602 Bad request: the security token has expired"));
        assert!(is_auth_error("APIG.0301 verify ak sk signature fail"));
        assert!(!is_auth_error("TM.00001041"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gate_separates_stream_and_plain() {
        let gate = AccountGate::new(2);
        // 两个流式槽位占满
        let s1 = gate.acquire(true).await.unwrap();
        let _s2 = gate.acquire(true).await.unwrap();
        let third = tokio::time::timeout(Duration::from_millis(120), gate.acquire(true)).await;
        assert!(third.is_err(), "流式槽位耗尽时应阻塞等待");

        // 非流式不受流式槽位影响
        let p = tokio::time::timeout(Duration::from_millis(500), gate.acquire(false))
            .await
            .expect("非流式请求不应被流式槽位阻塞")
            .unwrap();
        drop(p);
        drop(s1);
    }

    #[tokio::test]
    async fn concurrency_error_sets_cooldown() {
        let gate = AccountGate::new(3);
        assert!(gate.available().await);
        gate.note_failure("TM.00001041 并发会话数已达上限").await;
        assert!(!gate.available().await);
        assert!(gate.cooldown_secs_if_any().await.unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn stream_slot_is_held_after_drop() {
        // 流式槽位 drop 后仍处于冷却期，不应立刻空出来
        let gate = AccountGate::new(1);
        assert_eq!(gate.free_stream_slots(), 1);
        let guard = gate.acquire(true).await.unwrap();
        assert_eq!(gate.free_stream_slots(), 0);
        drop(guard);
        // 冷却期远大于测试窗口，因此这里应当仍是 0
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(gate.free_stream_slots(), 0, "流式槽位必须等冷却期结束才归还");
    }

    #[tokio::test]
    async fn pin_roundtrip() {
        let pins = SessionPins::new();
        assert!(pins.get("s1").await.is_none());
        pins.pin("s1", "acc-a").await;
        assert_eq!(pins.get("s1").await.as_deref(), Some("acc-a"));
        pins.release_account("acc-a").await;
        assert!(pins.get("s1").await.is_none());
    }

    #[test]
    fn session_key_prefers_header() {
        let body = json!({"messages":[{"role":"user","content":"hello"}]});
        assert_eq!(session_key(&body, Some("chat-123")).as_deref(), Some("hdr:chat-123"));
        assert_eq!(session_key(&body, Some("")).unwrap().starts_with("msg:"), true);
    }

    #[test]
    fn session_key_stable_for_same_first_message() {
        let a = json!({"messages":[
            {"role": "user", "content": "写一个排序函数"},
            {"role": "assistant", "content": "好的"}
        ]});
        let b = json!({"messages":[
            {"role": "user", "content": "写一个排序函数"},
            {"role": "assistant", "content": "好的"},
            {"role": "user", "content": "换成降序"}
        ]});
        assert_eq!(session_key(&a, None), session_key(&b, None));
    }

    #[test]
    fn session_key_handles_multimodal_content() {
        // 多模态首条消息不应退化成 None（旧实现会）
        let body = json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"这张图里是什么"},
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}
        ]}]});
        let key = session_key(&body, None);
        assert!(key.is_some(), "多模态内容也必须能派生出会话键");
        assert!(key.unwrap().starts_with("msg:"));
    }

    #[test]
    fn upstream_ids_are_stable_and_distinct() {
        let a = upstream_chat_id("msg:abc");
        let b = upstream_chat_id("msg:abc");
        let c = upstream_chat_id("msg:def");
        assert_eq!(a, b, "同一会话键必须得到同一 chat_id");
        assert_ne!(a, c, "不同会话必须得到不同 chat_id");
        assert!(a.starts_with("chat-"));

        let sid = upstream_session_id(&a);
        assert_eq!(sid.len(), 32); // 16 字节 hex
        assert_eq!(sid, upstream_session_id(&a));
    }

    #[test]
    fn session_key_uses_user_field() {
        let body = json!({"user":"u42","messages":[{"role":"user","content":"x"}]});
        assert_eq!(session_key(&body, None).as_deref(), Some("user:u42"));
    }
}
