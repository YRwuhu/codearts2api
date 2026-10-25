//! 会话钉号与并发控制。
//!
//! 两个要解决的问题：
//!
//! 1. **会话槽位泄漏**：上游按"会话"计并发（当前账号上限 3）。如果每次请求
//!    都被当成新会话，短时间内连续请求就会耗尽槽位并收到
//!    `TM.00001041 并发会话数已达上限`。把同一对话的请求固定到同一账号、
//!    并复用上游 conversation id，可以让上游识别为同一会话。
//!
//! 2. **本地并发闸门**：即使上游限 3，代理侧也不该毫不知情地把请求全打出去。
//!    这里给每个账号一个信号量，超过上限就排队等槽位，而不是直接失败。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, RwLock, Semaphore};

/// 会话钉号的有效期；超过后重新选号。
const PIN_TTL: Duration = Duration::from_secs(30 * 60);

/// 单个账号的并发闸门与健康状态。
pub struct AccountGate {
    /// 上游允许的并发数（默认 3）
    limit: usize,
    semaphore: Arc<Semaphore>,
    state: Arc<RwLock<GateState>>,
}

#[derive(Default)]
struct GateState {
    in_flight: u32,
    /// 冷却截止时间（触发并发上限后短暂避让）
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    total_requests: u64,
    total_failures: u64,
    last_error: Option<String>,
}

impl AccountGate {
    pub fn new(limit: usize) -> Self {
        let limit = limit.max(1);
        Self {
            limit,
            semaphore: Arc::new(Semaphore::new(limit)),
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

    /// 获取一个并发槽位（必要时排队等待）。
    ///
    /// 冷却期内会**等待冷却结束**再取槽位，而不是立刻失败——只有一个账号时
    /// 立即失败等于把请求拒掉。只有等待超时（由调用方控制）才返回错误。
    ///
    /// 返回 guard 时 in_flight 已加 1；drop 时自动释放。
    pub async fn acquire(&self) -> Result<OwnedPermitGuard, String> {
        if let Some(secs) = self.cooldown_secs_if_any().await {
            if secs > 0 {
                tokio::time::sleep(Duration::from_secs(secs.min(30))).await;
            }
        }

        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "并发闸门已关闭".to_string())?;

        {
            let mut st = self.state.write().await;
            st.in_flight += 1;
            st.total_requests += 1;
        }

        Ok(OwnedPermitGuard {
            permit,
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
            // 并发上限：避让 20 秒，让上游释放槽位
            st.cooldown_until = Some(Instant::now() + Duration::from_secs(20));
        } else if st.consecutive_failures >= 3 {
            // 连续失败：指数避让，最长 5 分钟
            let secs = (10u64 * st.consecutive_failures as u64).min(300);
            st.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    /// 若在冷却中返回剩余秒数。
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
        let st = self.state.read().await;
        serde_json::json!({
            "limit": self.limit,
            "in_flight": st.in_flight,
            "available": st.cooldown_until.map(|u| Instant::now() >= u).unwrap_or(true),
            "cooldown_remaining": st.cooldown_until
                .map(|u| if Instant::now() >= u { 0 } else { (u - Instant::now()).as_secs() })
                .unwrap_or(0),
            "consecutive_failures": st.consecutive_failures,
            "total_requests": st.total_requests,
            "total_failures": st.total_failures,
            "last_error": st.last_error,
        })
    }
}

pub struct OwnedPermitGuard {
    permit: tokio::sync::OwnedSemaphorePermit,
    state: Arc<RwLock<GateState>>,
}

impl Drop for OwnedPermitGuard {
    fn drop(&mut self) {
        // 释放 permit
        let _ = &self.permit;
        // in_flight 需要在异步上下文里减；用 try_write 避免 drop 中 await
        if let Ok(mut st) = self.state.try_write() {
            st.in_flight = st.in_flight.saturating_sub(1);
        } else {
            // 拿不到锁时把 permit 交给一个后台任务处理
            let state = self.state.clone();
            tokio::spawn(async move {
                let mut st = state.write().await;
                st.in_flight = st.in_flight.saturating_sub(1);
            });
        }
    }
}

/// 上游并发上限错误的判定。
pub fn is_concurrency_error(text: &str) -> bool {
    text.contains("TM.00001041") || text.contains("并发会话数")
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
/// 上游用它识别"这是同一个会话"，从而复用同一个并发槽位而不是新开一个。
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
/// 否则退化为"首条 user 消息内容的哈希"——同一对话的续轮通常带相同历史，
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

    // 退化：取第一条 user 消息的哈希（同一对话首条消息固定）
    let messages = body.get("messages")?.as_array()?;
    let first_user = messages
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;
    let text = first_user.get("content")?.as_str()?;
    if text.is_empty() {
        return None;
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hex::encode(hasher.finalize());
    Some(format!("msg:{}", &digest[..16]))
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

    #[tokio::test]
    async fn gate_limits_concurrency() {
        let gate = AccountGate::new(2);
        let p1 = gate.acquire().await.unwrap();
        let _p2 = gate.acquire().await.unwrap();
        // 第三个槽位应立即失败（用 try 语义：这里用超时判断）
        let third = tokio::time::timeout(Duration::from_millis(120), gate.acquire()).await;
        assert!(third.is_err(), "超出上限时应阻塞等待");
        drop(p1);
        // 释放后可以拿到
        let _p3 = tokio::time::timeout(Duration::from_millis(500), gate.acquire())
            .await
            .expect("释放后应能获取")
            .unwrap();
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
            {"role":"user","content":"写一个排序函数"},
            {"role":"assistant","content":"好的"}
        ]});
        let b = json!({"messages":[
            {"role":"user","content":"写一个排序函数"},
            {"role":"assistant","content":"好的"},
            {"role":"user","content":"换成降序"}
        ]});
        assert_eq!(session_key(&a, None), session_key(&b, None));
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
