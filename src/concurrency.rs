//! 账号级会话标识与在途请求节流。
//!
//! # 上游到底怎么限并发（实测，2026-10）
//!
//! 上游按 **`x-ot-session-id` / `user-session-id`** 识别会话，每个账号最多
//! 同时存在 3 个会话。请求不带这两个头时，上游把**每一个请求都当成新会话**，
//! 于是连续 3 次之后第 4 次必被拒——即使这些请求完全串行。
//!
//! ## 修复方式：所有请求共用一个账号级会话 ID
//!
//! 让同一账号下的所有请求带同一个固定 `x-ot-session-id`（见
//! [`account_session_id`]），该账号就永远只占 1 个上游会话，与本地开几个
//! 对话、几个 sub agent、几个并发流都无关。
//!
//! 实测对照（同一账号）：
//!
//! | 场景 | 结果 |
//! |---|---|
//! | 8 个串行流式请求，不带会话头 | 3 个成功，后 5 个被拒 |
//! | 8 个串行流式请求，带固定会话头 | 8/8 成功 |
//! | 5 个并发流式请求，带固定会话头 | 5/5 成功 |
//! | 8 个并发流式请求，带固定会话头 | 8/8 成功 |
//!
//! 因此「每分钟只能开 3 个流式会话」的旧结论是**错误的表象**：那是「每个
//! 请求都被当成新会话，而会话存活约 60 秒」的观测结果，不是速率限制。
//! 加上会话头之后限制完全消失，也不再需要靠「工具轮降级非流式」来省额度。
//!
//! ## 那还需要本地闸门吗
//!
//! 需要，但目的变了：不是限制「会话数」（会话已恒为 1），而是限制**在途
//! 请求数**，避免本地把上游打爆或耗尽连接。上限给得较宽松，正常使用不排队。
//!
//! `chat_id` / `Chat-Id` / `Session-Id` / `prompt_cache_key` 仍然保留，但
//! 它们只用于上游对话识别与 prompt cache 亲和，**不是**并发键。
//!
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};

/// 触发并发错误后的冷却时间。
///
/// 正常情况下不应该再遇到该错误（账号级会话头已把它解决）。真遇到时说明
/// 有别处也在用同一账号，退避一阵避免继续撞墙。
pub const SATURATION_COOLDOWN: Duration = Duration::from_secs(30);

/// 会话钉号的有效期；超过后重新选号。
const PIN_TTL: Duration = Duration::from_secs(30 * 60);

/// 单个账号的流式限流器与健康状态。
pub struct AccountGate {
    /// 本地在途流式请求上限（不是上游会话数——会话已经恒为 1）
    limit: usize,
    /// 在途流式请求
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

    /// 还有几个空闲的流式并发位。
    pub async fn free_stream_slots(&self) -> usize {
        self.stream_concurrent.available_permits()
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

        // 占一个在途并发位。上游会话已由 x-ot-session-id 收敛为 1 个，
        // 这里纯粹是本地保护。
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
            st.cooldown_until = Some(Instant::now() + SATURATION_COOLDOWN);
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
        let free = self.stream_concurrent.available_permits();
        let st = self.state.read().await;
        serde_json::json!({
            "limit": self.limit,
            "in_flight": st.stream_in_flight,
            "stream_in_flight": st.stream_in_flight,
            "plain_in_flight": st.plain_in_flight,
            "stream_free_slots": free,
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

/// **账号级**上游会话 ID。
///
/// 这是 `TM.00001041 并发会话数已达上限(3个)` 的真正解法。
///
/// 上游按 `x-ot-session-id` / `user-session-id` 识别会话，每个账号最多
/// 同时存在 3 个会话。请求不带这两个头时，上游把**每一个请求都当成新会话**，
/// 于是连续 3 次之后第 4 次必被拒——即使这些请求完全串行。
///
/// 让同一账号下的所有请求共用一个固定会话 ID 后，该账号永远只占 1 个会话，
/// 与本地开几个对话、几个 sub agent、几个并发流都无关。
///
/// 实测对照（同一账号、8 个串行流式请求）：
/// - 不带会话头：3 个成功，后 5 个 `TM.00001041`
/// - 带固定会话头：**8/8 全部成功**；5 个并发流式请求同样 5/5 成功
///
/// 命名沿用 Python 版的 `ses_` 前缀。派生用 `seed`（OAuth 账号传入稳定的
/// user_id，手动账号传入 AK），避免 STS 临时凭证续期后会话 ID 漂移。
pub fn account_session_id(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    format!("ses_{}", &hex::encode(hasher.finalize())[..24])
}

/// 把本地会话键转成稳定的上游 chat_id。
///
/// 注意：这**不是**并发键（并发由 [`account_session_id`] 决定），只为让
/// 同账号上的同一对话命中 prompt cache、并在日志里可追踪。
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
    async fn stream_concurrency_is_capped_locally() {
        let gate = AccountGate::new(3);
        let _a = gate.acquire(true).await.unwrap();
        let _b = gate.acquire(true).await.unwrap();
        let _c = gate.acquire(true).await.unwrap();
        let fourth = tokio::time::timeout(Duration::from_millis(150), gate.acquire(true)).await;
        assert!(fourth.is_err(), "本地在途并发超过上限时应阻塞");
    }

    #[tokio::test]
    async fn sequential_streams_never_wait() {
        // 会话头修复后，串行请求不应有任何人为延迟（旧实现会卡 60 秒）
        let gate = AccountGate::new(3);
        for i in 0..12 {
            let g = tokio::time::timeout(Duration::from_millis(300), gate.acquire(true))
                .await
                .unwrap_or_else(|_| panic!("第 {i} 个串行流被不必要地阻塞了"))
                .unwrap();
            drop(g);
        }
    }

    #[tokio::test]
    async fn dropped_stream_frees_its_slot_immediately() {
        let gate = AccountGate::new(1);
        let g = gate.acquire(true).await.unwrap();
        assert_eq!(gate.free_stream_slots().await, 0);
        drop(g);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            gate.free_stream_slots().await,
            1,
            "上游会话已由会话头收敛，本地并发位应立即归还"
        );
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
    fn account_session_id_is_stable_and_prefixed() {
        let a = account_session_id("HPUATESTAK");
        let b = account_session_id("HPUATESTAK");
        assert_eq!(a, b, "同一账号必须得到同一会话 ID");
        assert!(a.starts_with("ses_"));
        assert_eq!(a.len(), 4 + 24);
        assert_ne!(a, account_session_id("OTHER"));
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
