//! 华为云 CodeArts OAuth2 登录（PKCE + DPoP）。
//!
//! 流程（逆向自官方 huaweicloud.authentication 扩展，并与社区实现交叉验证）：
//!   1. 生成 ticket_id / secret / PKCE / DPoP 密钥对
//!   2. 构造 `codearts.huaweicloud.com/portal/authorize` 链接，浏览器打开登录
//!   3. 双通道取凭据：
//!      a. 本地回调 `http://127.0.0.1:{port}/oauth/callback` 收到 code
//!         → POST sts/v1/oauth2/tokens (grant_type=authorization_code)
//!      b. 兜底：轮询 snap-manager `/v1/login/ticket`
//!   4. 得到 STS 临时 AK/SK + security_token + refresh_token，持久化
//!   5. 到期前用 refresh_token + 同一 DPoP 私钥续期

pub mod dpop;
pub mod flow;
pub mod store;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

pub use dpop::{pkce, DpopKey};

// ── 常量（来自官方插件逆向） ──────────────────────────────
pub const SNAP_MANAGER: &str = "https://snap-access.cn-north-4.myhuaweicloud.com/snap-manager";
pub const PORTAL: &str = "https://codearts.huaweicloud.com/portal";
pub const STS: &str = "https://sts.cn-north-4.myhuaweicloud.com";
pub const CLIENT_ID: &str = "codearts-agent";
pub const REDIRECT_PATH: &str = "/oauth/callback";
pub const PLUGIN_NAME: &str = "snap_AIIDE";
pub const PLUGIN_VERSION: &str = "5.2.0";

const EP_OAUTH_TOKENS: &str = "/v1/oauth2/tokens";
const EP_LOGIN_TICKET: &str = "/v1/login/ticket";

/// 登录换到的 STS 临时凭证 + 续期材料。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Identity {
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub domain_id: String,
    /// STS 临时 AK
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    /// 安全令牌，签名时需带 X-Security-Token
    #[serde(default)]
    pub security_token: String,
    /// RFC3339
    #[serde(default)]
    pub expiration: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub code_verifier: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub dpop_private_key: Option<DpopKey>,
    #[serde(default = "now")]
    pub updated_at: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Identity {
    pub fn is_complete(&self) -> bool {
        !self.access_key_id.is_empty()
            && !self.secret_access_key.is_empty()
            && !self.security_token.is_empty()
    }

    /// 剩余有效期；无过期时间时按 24 小时处理。
    pub fn remaining_secs(&self) -> i64 {
        match chrono::DateTime::parse_from_rfc3339(&self.expiration) {
            Ok(t) => (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds(),
            Err(_) => 24 * 3600,
        }
    }

    pub fn expiring_within(&self, secs: i64) -> bool {
        let r = self.remaining_secs();
        r > 0 && r <= secs
    }

    pub fn expired(&self) -> bool {
        self.remaining_secs() <= 0
    }

    /// 展示用：用户名或打码的 user_id
    pub fn display_name(&self) -> String {
        if !self.user_name.is_empty() {
            return self.user_name.clone();
        }
        if self.user_id.len() > 8 {
            return format!("{}****", &self.user_id[..8]);
        }
        if self.user_id.is_empty() {
            "未命名账号".to_string()
        } else {
            self.user_id.clone()
        }
    }
}

// ── 登录会话 ──────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub auth_url: String,
    pub callback_url: String,
    pub created_at: i64,
    pub done: bool,
    pub error: Option<String>,
    pub user_name: Option<String>,
}

pub struct Session {
    pub ticket_id: String,
    pub secret: String,
    pub verifier: String,
    pub dpop: DpopKey,
    pub port: u16,
    pub auth_url: String,
    pub created_at: i64,
    pub done: bool,
    pub error: Option<String>,
    pub identity: Option<Identity>,
}

/// 登录会话表。
#[derive(Clone, Default)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, Session>>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn put(&self, id: String, session: Session) {
        let mut guard = self.inner.lock().await;
        // 清理超过 15 分钟的旧会话
        let cutoff = now() - 900;
        guard.retain(|_, s| s.created_at > cutoff);
        guard.insert(id, session);
    }

    pub async fn info(&self, id: &str) -> Option<SessionInfo> {
        let guard = self.inner.lock().await;
        guard.get(id).map(|s| SessionInfo {
            session_id: id.to_string(),
            auth_url: s.auth_url.clone(),
            callback_url: format!("http://127.0.0.1:{}{}", s.port, REDIRECT_PATH),
            created_at: s.created_at,
            done: s.done,
            error: s.error.clone(),
            user_name: s.identity.as_ref().map(|i| i.display_name()),
        })
    }

    pub async fn mark_done(&self, id: &str, identity: Identity) {
        let mut guard = self.inner.lock().await;
        if let Some(s) = guard.get_mut(id) {
            s.done = true;
            s.identity = Some(identity);
        }
    }

    pub async fn mark_failed(&self, id: &str, error: String) {
        let mut guard = self.inner.lock().await;
        if let Some(s) = guard.get_mut(id) {
            s.error = Some(error);
        }
    }

    /// 按 ticket_id 更新 portal 回传的 secret（回调里可能替换）。
    pub async fn set_secret_by_ticket(&self, ticket_id: &str, secret: &str) {
        let mut guard = self.inner.lock().await;
        for s in guard.values_mut() {
            if s.ticket_id == ticket_id {
                s.secret = secret.to_string();
            }
        }
    }

    /// 取最近创建、且尚未完成的会话（回调 URL 不带 ticket_id 时按此匹配）。
    pub async fn latest_pending(&self) -> Option<(String, String, String, DpopKey, u16)> {
        let guard = self.inner.lock().await;
        guard
            .values()
            .filter(|s| !s.done && s.error.is_none())
            .max_by_key(|s| s.created_at)
            .map(|s| {
                (
                    s.ticket_id.clone(),
                    s.secret.clone(),
                    s.verifier.clone(),
                    s.dpop.clone(),
                    s.port,
                )
            })
    }

    /// 按 session_id 取 ticket 轮询所需材料。
    pub async fn find_by_ticket_by_session(
        &self,
        session_id: &str,
    ) -> Option<(String, String, String, DpopKey, u16)> {
        let guard = self.inner.lock().await;
        guard.get(session_id).map(|s| {
            (
                s.ticket_id.clone(),
                s.secret.clone(),
                s.verifier.clone(),
                s.dpop.clone(),
                s.port,
            )
        })
    }

    pub async fn find_by_ticket(&self, ticket_id: &str) -> Option<(String, String, String, DpopKey, u16)> {
        let guard = self.inner.lock().await;
        guard
            .iter()
            .find(|(_, s)| s.ticket_id == ticket_id)
            .map(|(id, s)| {
                (
                    id.clone(),
                    s.secret.clone(),
                    s.verifier.clone(),
                    s.dpop.clone(),
                    s.port,
                )
            })
    }
}

/// 构造授权链接。
pub fn build_authorize_url(ticket_id: &str, challenge: &str, port: u16) -> String {
    let params = [
        ("theme", "2".to_string()),
        ("locale", "zh-cn".to_string()),
        ("uri_scheme", CLIENT_ID.to_string()),
        ("client_id", CLIENT_ID.to_string()),
        ("port", port.to_string()),
        ("code_challenge", challenge.to_string()),
        ("code_challenge_method", "SHA-256".to_string()),
        ("ticket_id", ticket_id.to_string()),
        ("plugin-name", PLUGIN_NAME.to_string()),
        ("plugin-version", PLUGIN_VERSION.to_string()),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{PORTAL}/authorize?{query}")
}

fn urlencode(v: &str) -> String {
    url::form_urlencoded::byte_serialize(v.as_bytes()).collect()
}

#[derive(Deserialize, Default)]
struct TokenPayload {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    user_name: String,
    #[serde(default)]
    domain_id: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    credentials: Credentials,
    #[serde(default)]
    credential: LegacyCredential,
    #[serde(default)]
    #[allow(dead_code)]
    error: String,
    #[serde(default)]
    error_code: String,
    #[serde(default)]
    error_msg: String,
}

#[derive(Deserialize, Default)]
struct Credentials {
    #[serde(default)]
    access_key_id: String,
    #[serde(default)]
    secret_access_key: String,
    #[serde(default)]
    security_token: String,
    #[serde(default)]
    expiration: String,
}

#[derive(Deserialize, Default)]
struct LegacyCredential {
    #[serde(default)]
    access: String,
    #[serde(default)]
    secret: String,
    #[serde(default)]
    securitytoken: String,
    #[serde(default)]
    expires_at: String,
}

impl TokenPayload {
    fn into_identity(self, verifier: &str, dpop: &DpopKey) -> Option<Identity> {
        let (ak, sk, tok, exp) = if !self.credentials.security_token.is_empty() {
            (
                self.credentials.access_key_id,
                self.credentials.secret_access_key,
                self.credentials.security_token,
                self.credentials.expiration,
            )
        } else if !self.credential.securitytoken.is_empty() {
            (
                self.credential.access,
                self.credential.secret,
                self.credential.securitytoken,
                self.credential.expires_at,
            )
        } else {
            return None;
        };

        Some(Identity {
            user_id: self.user_id,
            user_name: self.user_name,
            domain_id: self.domain_id,
            access_key_id: ak,
            secret_access_key: sk,
            security_token: tok,
            expiration: exp,
            refresh_token: self.refresh_token,
            code_verifier: verifier.to_string(),
            client_id: CLIENT_ID.to_string(),
            dpop_private_key: Some(dpop.clone()),
            updated_at: now(),
        })
    }
}

/// 用授权码换 token。
pub async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
    port: u16,
    dpop: &DpopKey,
) -> Result<Identity, String> {
    let url = format!("{STS}{EP_OAUTH_TOKENS}");
    let redirect = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");
    let form = [
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect.as_str()),
    ];

    let proof = dpop.proof(&url)?;
    let resp = http
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("DPoP", proof)
        .form(&form)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("token 请求失败: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let payload: TokenPayload = serde_json::from_str(&text).unwrap_or_default();

    if !status.is_success() {
        return Err(format!(
            "HTTP {status}: {} {}",
            payload.error_code,
            if !payload.error_msg.is_empty() {
                payload.error_msg
            } else {
                text.chars().take(200).collect()
            }
        ));
    }

    payload
        .into_identity(verifier, dpop)
        .ok_or_else(|| format!("token 响应缺少凭证: {}", text.chars().take(200).collect::<String>()))
}

/// 用 STS 凭证查账号身份（登录响应有时不带 user_id/user_name）。
pub async fn fetch_current_user(
    http: &reqwest::Client,
    identity: &Identity,
) -> Result<(String, String, String), String> {
    let url = format!("{SNAP_MANAGER}/v1/current/user");
    let signer = crate::signer::Signer::new(&identity.access_key_id, &identity.secret_access_key);
    let signed = signer.sign_full(
        "GET",
        &url,
        b"",
        &[
            ("content-type", "application/json"),
            ("x-language", "zh-cn"),
            ("accept", "application/json"),
        ],
        Some(&identity.security_token),
    );

    let mut req = http.get(&url);
    for (k, v) in signed.headers {
        req = req.header(k, v);
    }

    let resp = req
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("查询身份失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("查询身份 HTTP {}", resp.status()));
    }

    let value: Value = resp.json().await.map_err(|e| format!("解析身份失败: {e}"))?;
    // 兼容直接返回与包在 data 里两种形态
    let node = value.get("data").unwrap_or(&value);
    Ok((
        node.get("user_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        node.get("user_name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        node.get("domain_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
    ))
}

/// 用 refresh_token 续期（必须复用签发时的 DPoP 私钥与 code_verifier）。
pub async fn refresh(http: &reqwest::Client, identity: &Identity) -> Result<Identity, String> {
    let Some(dpop) = identity.dpop_private_key.clone() else {
        return Err("缺少 DPoP 私钥，无法续期，请重新登录".to_string());
    };
    if identity.refresh_token.is_empty() {
        return Err("缺少 refresh_token，请重新登录".to_string());
    }

    let url = format!("{STS}{EP_OAUTH_TOKENS}");
    let form = [
        ("client_id", CLIENT_ID),
        ("code_verifier", identity.code_verifier.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", identity.refresh_token.as_str()),
    ];

    let proof = dpop.proof(&url)?;
    let resp = http
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("DPoP", proof)
        .form(&form)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("续期请求失败: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let payload: TokenPayload = serde_json::from_str(&text).unwrap_or_default();

    if !status.is_success() {
        return Err(format!(
            "续期失败 HTTP {status}: {} {}",
            payload.error_code, payload.error_msg
        ));
    }

    let mut updated = payload
        .into_identity(&identity.code_verifier, &dpop)
        .ok_or_else(|| "续期响应缺少凭证".to_string())?;
    // refresh_token 可能轮转，也可能不变
    if updated.refresh_token.is_empty() {
        updated.refresh_token = identity.refresh_token.clone();
    }
    if updated.user_id.is_empty() {
        updated.user_id = identity.user_id.clone();
        updated.user_name = identity.user_name.clone();
        updated.domain_id = identity.domain_id.clone();
    }
    Ok(updated)
}

/// 轮询 ticket（兜底通道，用于浏览器无法回调本机的场景）。
pub async fn poll_ticket(
    http: &reqwest::Client,
    ticket_id: &str,
    secret: &str,
    verifier: &str,
    dpop: &DpopKey,
) -> Result<Option<Identity>, String> {
    let url = format!(
        "{SNAP_MANAGER}{EP_LOGIN_TICKET}?ticket_id={}&secret={}",
        urlencode(ticket_id),
        urlencode(secret)
    );
    let resp = http
        .get(&url)
        .header("plugin-name", PLUGIN_NAME)
        .header("plugin-version", PLUGIN_VERSION)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("ticket 查询失败: {e}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }
    let value: Value = resp.json().await.unwrap_or(Value::Null);
    if value.get("error_code").is_some() {
        return Ok(None);
    }

    let payload: TokenPayload = serde_json::from_value(value).unwrap_or_default();
    if payload.user_name.is_empty() && payload.user_id.is_empty() {
        return Ok(None);
    }
    Ok(payload.into_identity(verifier, dpop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_authorize_url_with_required_params() {
        let url = build_authorize_url("tid123", "chal456", 51234);
        assert!(url.starts_with("https://codearts.huaweicloud.com/portal/authorize?"));
        for expected in [
            "client_id=codearts-agent",
            "uri_scheme=codearts-agent",
            "code_challenge=chal456",
            "code_challenge_method=SHA-256",
            "ticket_id=tid123",
            "port=51234",
            "plugin-name=snap_AIIDE",
        ] {
            assert!(url.contains(expected), "missing {expected} in {url}");
        }
    }

    #[test]
    fn identity_completeness() {
        let mut id = Identity::default();
        assert!(!id.is_complete());
        id.access_key_id = "ak".into();
        id.secret_access_key = "sk".into();
        assert!(!id.is_complete());
        id.security_token = "tok".into();
        assert!(id.is_complete());
    }

    #[test]
    fn expiration_helpers() {
        let mut id = Identity::default();
        // 没有过期时间时默认为 24 小时
        assert!(id.remaining_secs() > 0);
        assert!(!id.expired());

        id.expiration = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(id.expiring_within(2 * 3600));
        assert!(!id.expiring_within(60));

        id.expiration = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(id.expired());
    }

    #[test]
    fn display_name_prefers_user_name() {
        let mut id = Identity::default();
        assert_eq!(id.display_name(), "未命名账号");
        id.user_id = "0123456789abcdef".into();
        assert_eq!(id.display_name(), "01234567****");
        id.user_name = "hid_tuj23".into();
        assert_eq!(id.display_name(), "hid_tuj23");
    }
}
