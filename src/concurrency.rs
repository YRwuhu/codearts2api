//! 会话槽位管理。
//!
//! # 上游到底怎么限流（实测，2026-10）
//!
//! 对同一个 CodeArts 账号打 `/api/v2/chat/completions`：
//!
//! | 请求形态 | 表现 |
//! |---|---|
//! | `stream=false` | 并发 4 个全部成功，基本不限 |
//! | `stream=true`  | 同时最多 3 个，第 4 个 `TM.00001041` |
//!
//! 几个容易误判的点，都已实测排除：
//!
//! - `chat_id` / `Chat-Id` / `Session-Id` / `prompt_cache_key` 都**不能**
//!   复用槽位。同一 chat_id 连发 6 次，仍然在第 4 次被拒。
//! - 不是绑定 TCP 连接：每请求新建连接、或单连接 keep-alive 连发，行为一致。
//! - 上游不按 chat_id 累积历史（`prompt_tokens` 恒定），所以这与服务端会话
//!   记忆无关。
//!
//! # 槽位的准确模型（实测）
//!
//! 连续对照实验（每次先静默 130 秒确保干净）：
//!
//! | 场景 | 结果 |
//! |---|---|
//! | 1 个流跑完 | 8 秒后新流就 OK |
//! | 2 个顺序流（不并发） | 6 秒后新流就 OK |
//! | 3 个顺序流 | 58 秒后才恢复 |
//! | 3 个**并发**流（各跑 1.5 秒） | 结束后 58 秒内新流全被拒 |
//! | 3 个并发流（各跑 35 秒） | 从**启动**算 t+68s 恢复 |
//!
//! 这两个数据点把模型钉死了：长流耗尽槽位后，恢复时间是「启动后 ~60 秒」，
//! 而不是「结束后 ~60 秒」（后者要等到 t≈95s）。所以约束是：
//!
//! **每 60 秒窗口内最多启动 3 个流式会话。**
//!
//! 这是一个**速率**限制，不是单纯的并发限制——第 4 个顺序流即使前 3 个
//! 早就结束了，也照样被拒。因此单个对话如果在一分钟内跑超过 3 轮，
//! 第 4 轮必然要等窗口滑过去。这是上游产品限制，代理无法绕过；
//! 想提高吞吐只能加账号（每账号各算各的窗口）。
//!
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};

/// 上游流式会话的滑动窗口长度。
///
/// 实测：
/// - 3 个短流（各 1.5 秒）→ 新流在 t+58s 恢复
/// - 3 个长流（各 35 秒）→ 新流在 t+68s 恢复
///
/// 两者都落在「启动后约 60 秒」，因此按**启动时间**计窗口。
pub const STREAM_WINDOW: Duration = Duration::from_secs(60);

/// 会话钉号的有效期；超过后重新选号。
const PIN_TTL: Duration = Duration::from_secs(30 * 60);

/// 单个账号的流式限流器与健康状态。
pub struct AccountGate {
    /// 每个窗口内允许启动的流式会话数（实测 3）
    limit: usize,
    /// 最近一个窗口内的流式启动时间
    stream_starts: Arc<Mutex<VecDeque<Instant>>>,
    /// 同时在飞的流式请求（窗口过期后仍可能有流在跑）
    stream_concurrent: Arc<Semaphore>,
    /// 非流式并发（上游对这种请求几乎不限，但本地仍限个上限防止打爆）
    plain_slots: Arc<Semaphore>,
    state: Arc<RwLock<GateState>>,
}

#[derive(Default)]
struct GateState {
    /// 当前流式请求数
    stream_in_flight: u32,
    plain_in_flight: u32,
    /// 冷却截止时间（触发并发上限后短暂避让）
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    total_requests: u64,
    total_failures: u64,
    /// 按模型隔离的冷却（账号级冷却会让整个账号不可用，模型级只挡那一个模型）
    model_cooldowns: HashMap<String, Instant>,
    /// 当日 token 消耗与配额
    daily_tokens: i64,
    /// 日切时间戳（本地日期 0 点的秒级时间）
    daily_reset_ts: i64,
    last_error: Option<String>,
}

impl AccountGate {
    pub fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            limit,
            stream_starts: Arc::new(Mutex::new(VecDeque::new())),
            stream_concurrent: Arc::new(Semaphore::new(limit)),
            // 非流式请求不占上游会话槽，但限制一下并发避免把带宽打满
            plain_slots: Arc::new(Semaphore::new(limit.saturating_mul(4).max(4))),
            state: Arc::new(RwLock::new(GateState::default())),
        }
    }

    /// 该账号现在是否可用（不在账号级冷却中）。
    pub async fn available(&self) -> bool {
        let st = self.state.read().await;
        match st.cooldown_until {
            Some(until) => Instant::now() >= until,
            None => true,
        }
    }

    /// 当前窗口内还剩几个流式名额。
    pub async fn free_stream_slots(&self) -> usize {
        let mut q = self.stream_starts.lock().await;
        prune_window(&mut q);
        self.limit.saturating_sub(q.len())
    }

    /// 获取一个槽位。
    ///
    /// 流式：先在滑动窗口里预约名额（不够就等到窗口里最老的那个过期），
    /// 再占用一个并发许可。非流式：只占本地并发许可，上游不限制。
    pub async fn acquire(&self, stream: bool) -> Result<SlotGuard, String> {
        if !stream {
            let permit = self
                .plain_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| "并发闸门已关闭".to_string())?;
            {
                let mut st = self.state.write().await;
                st.plain_in_flight += 1;
                st.total_requests += 1;
            }
            return Ok(SlotGuard {
                permit: Some(permit),
                stream: false,
                state: self.state.clone(),
            });
        }

        // 1) 在当前窗口里预约一个名额；满了就等到最早那次启动滑出窗口。
        loop {
            let deadline = {
                let mut q = self.stream_starts.lock().await;
                prune_window(&mut q);
                if q.len() < self.limit {
                    q.push_back(Instant::now());
                    None
                } else {
                    q.front().map(|t| *t + STREAM_WINDOW)
                }
            };
            let Some(deadline) = deadline else { break };
            let now = Instant::now();
            if deadline > now {
                tokio::time::sleep(deadline - now).await;
            }
        }

        // 2) 并发许可：窗口过期后旧流可能还在跑，这里再兜一层。
        let permit = self
            .stream_concurrent
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "并发闸门已关闭".to_string())?;
        {
            let mut st = self.state.write().await;
            st.stream_in_flight += 1;
            st.total_requests += 1;
        }

        Ok(SlotGuard {
            permit: Some(permit),
            stream: true,
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
            // 说明本地记账落后于上游（例如别处也在用同一账号），退避一阵
            st.cooldown_until = Some(Instant::now() + STREAM_WINDOW);
        } else if st.consecutive_failures >= 3 {
            let secs = (10u64 * st.consecutive_failures as u64).min(300);
            st.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
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

    /// 快照，供 WebUI 展示。
    pub async fn snapshot(&self) -> serde_json::Value {
        let now = Instant::now();
        let used = {
            let mut q = self.stream_starts.lock().await;
            prune_window(&mut q);
            q.len()
        };
        let st = self.state.read().await;
        serde_json::json!({
            "limit": self.limit,
            "in_flight": st.stream_in_flight,
            "stream_in_flight": st.stream_in_flight,
            "plain_in_flight": st.plain_in_flight,
            "stream_used_in_window": used,
            "window_secs": STREAM_WINDOW.as_secs(),
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

/// 丢掉滑出窗口的启动记录。
fn prune_window(q: &mut VecDeque<Instant>) {
    let now = Instant::now();
    while let Some(front) = q.front() {
        if now.duration_since(*front) >= STREAM_WINDOW {
            q.pop_front();
        } else {
            break;
        }
    }
}

/// 占用的槽位。drop 时立即归还并发许可；窗口记录会自己过期。
pub struct SlotGuard {
    permit: Option<OwnedSemaphorePermit>,
    stream: bool,
    state: Arc<RwLock<GateState>>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
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
        // 并发许可立即归还；窗口里的启动记录负责「60 秒内最多 3 个」。
        drop(self.permit.take());
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

    #[tokio::test]
    async fn plain_requests_are_not_limited_by_stream_window() {
        let gate = AccountGate::new(2);
        let _s1 = gate.acquire(true).await.unwrap();
        let _s2 = gate.acquire(true).await.unwrap();
        assert_eq!(gate.free_stream_slots().await, 0);
        let p = tokio::time::timeout(Duration::from_millis(500), gate.acquire(false))
            .await
            .expect("非流式请求不应被流式窗口阻塞")
            .unwrap();
        drop(p);
    }

    #[tokio::test]
    async fn stream_concurrency_is_capped() {
        let gate = AccountGate::new(3);
        let _a = gate.acquire(true).await.unwrap();
        let _b = gate.acquire(true).await.unwrap();
        let _c = gate.acquire(true).await.unwrap();
        let fourth = tokio::time::timeout(Duration::from_millis(150), gate.acquire(true)).await;
        assert!(fourth.is_err(), "并发流超过 3 个时必须阻塞等待");
    }

    #[tokio::test]
    async fn dropped_stream_keeps_its_window_slot() {
        // 上游是速率限制：流结束后名额仍留在 60 秒窗口里
        let gate = AccountGate::new(1);
        let g = gate.acquire(true).await.unwrap();
        drop(g);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            gate.free_stream_slots().await,
            0,
            "流结束后名额仍应留在窗口里（上游 60 秒内不再接受第 4 个）"
        );
    }

    #[tokio::test]
    async fn first_three_streams_do_not_wait() {
        // 窗口内的前 3 个流必须立即放行，只有第 4 个才排队
        let gate = AccountGate::new(3);
        for i in 0..3 {
            let g = tokio::time::timeout(Duration::from_millis(300), gate.acquire(true))
                .await
                .unwrap_or_else(|_| panic!("第 {i} 个流被不必要地阻塞了"))
                .unwrap();
            drop(g);
        }
        assert_eq!(gate.free_stream_slots().await, 0);
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
    async fn daily_tokens_accumulate() {
        let gate = AccountGate::new(3);
        assert_eq!(gate.daily_tokens().await, 0);
        gate.add_daily_tokens(100).await;
        gate.add_daily_tokens(50).await;
        assert_eq!(gate.daily_tokens().await, 150);
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
        assert_eq!(sid.len(), 32);
        assert_eq!(sid, upstream_session_id(&a));
    }

    #[test]
    fn session_key_uses_user_field() {
        let body = json!({"user":"u42","messages":[{"role":"user","content":"x"}]});
        assert_eq!(session_key(&body, None).as_deref(), Some("user:u42"));
    }
}
