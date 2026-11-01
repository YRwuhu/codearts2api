//! 模型目录：云端同步 + 本地缓存 + 通道判定。
//!
//! 模型分两条上游通道：
//! - `agent`：老套餐模型，走 AgentCenter；
//! - `benefit`：免费额度模型，签名时必须多带 maas_type / model-id / model-name。
//!
//! 多账号场景下每个账号各持一个 `Catalog`，由 `Pool` 负责合并与故障切换。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Account;
use crate::signer::Signer;

pub const BASE_URL: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
pub const TARGET: &str = "https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions";
pub const AGENT_LIST_URL: &str =
    "https://snap-access.cn-north-4.myhuaweicloud.com/v1/agent-center/agents/useragents?offset=0&limit=100";
pub const AGENT_DETAIL_PATH: &str = "/v1/agent-center/agents/detail";
pub const OPENGW_BASE: &str = "https://opengw.developer.huaweicloud.com";
pub const MODEL_BUILTIN_URL: &str =
    "https://snap-access.cn-north-4.myhuaweicloud.com/v1/model/builtin";
pub const OPENGW_CONFIG_URL: &str =
    "https://opengw.developer.huaweicloud.com/api/v1/gateway/config";

pub const FALLBACK_AGENT_MODELS: [&str; 3] =
    ["openpangu-2.0-pro", "openpangu-2.0-flash", "GLM-5.2"];
pub const BENEFIT_MODELS: [&str; 3] = [
    "glm-5.3-flash",
    "deepseek-v4-pro-0813",
    "deepseek-v4-flash-0731",
];

const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default = "default_object")]
    pub object: String,
    #[serde(default)]
    pub owned_by: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub context_window: Option<i64>,
    #[serde(default)]
    pub max_tokens: Option<i64>,
    #[serde(default)]
    pub supports_images: bool,
    /// 可通过网页附件 + CloudAgent 会话处理图片，独立于直接内嵌图片能力。
    #[serde(default = "default_true")]
    pub supports_attachments: bool,
}

fn default_object() -> String {
    "model".to_string()
}
fn default_channel() -> String {
    "agent".to_string()
}

fn default_true() -> bool {
    true
}

/// 仅表示 `/api/v2/chat/completions` 接受内嵌图片的能力。
/// 网页附件 + CloudAgent 会话是另一条链路，不能用此字段否定其图片能力。
/// /v1/model/builtin 是该直接聊天能力的权威来源；这里仅作目录同步失败时的保守兜底。
fn supports_images(model_id: &str) -> bool {
    model_id.eq_ignore_ascii_case("kimi-k2.6-vl") || model_id.eq_ignore_ascii_case("qwen3-vl-235b")
}

fn builtin_model_values(value: &Value) -> Vec<&Value> {
    value
        .get("builtinModels")
        .and_then(|v| v.as_array())
        .or_else(|| value.as_array())
        .or_else(|| value.get("models").and_then(|v| v.as_array()))
        .or_else(|| value.get("data").and_then(|v| v.as_array()))
        .or_else(|| {
            value
                .get("data")
                .and_then(|v| v.get("models"))
                .and_then(|v| v.as_array())
        })
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

impl ModelInfo {
    fn new(id: String, channel: &str) -> Self {
        let supports_images = supports_images(&id);
        Self {
            name: Some(id.clone()),
            id,
            object: default_object(),
            owned_by: "codearts".to_string(),
            channel: channel.to_string(),
            description: None,
            context_window: None,
            max_tokens: None,
            supports_images,
            supports_attachments: true,
        }
    }

    pub fn to_openai(&self) -> Value {
        let mut obj = json!({
            "id": self.id,
            "object": "model",
            "created": 0,
            "owned_by": "codearts",
            "name": self.name.clone().unwrap_or_else(|| self.id.clone()),
            "description": self.description.clone().unwrap_or_default(),
            "supports_images": self.supports_images,
            "vision": self.supports_images,
            "supports_vision": self.supports_images,
            "supports_attachments": self.supports_attachments,
            "supports_function_calling": true,
            "supports_tool_calling": true,
            "supports_reasoning": true,
            "input_modalities": if self.supports_images { json!(["text","image"]) } else { json!(["text"]) },
            "output_modalities": ["text"],
            "supported_parameters": [
                "temperature", "top_p", "max_tokens", "stream",
                "tools", "tool_choice", "response_format"
            ],
        });
        if let Some(v) = self.context_window {
            obj["context_window"] = json!(v);
            obj["context_length"] = json!(v);
        }
        if let Some(v) = self.max_tokens {
            obj["max_tokens"] = json!(v);
            obj["max_output_tokens"] = json!(v);
        }
        obj
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    updated_at: i64,
    models: Vec<ModelInfo>,
}

struct CatalogState {
    models: BTreeMap<String, ModelInfo>,
    last_refresh: Option<std::time::Instant>,
}

/// 单个账号的模型目录与签名凭证。
pub struct Catalog {
    /// 稳定的账号 id，与凭证解耦（凭证会被续期替换）。
    id: String,
    /// 当前生效的账号凭证。OAuth 账号会在续期后整体替换。
    account: tokio::sync::RwLock<Account>,
    inner: Arc<tokio::sync::RwLock<CatalogState>>,
    cache_path: PathBuf,
    benefit_fallback: bool,
    http: reqwest::Client,
    /// 上一次成功续期的时间，避免并发请求重复续期。
    last_refresh_creds: std::sync::Mutex<Option<std::time::Instant>>,
    /// 自动与手动刷新共用一把锁，防止旧响应覆盖新目录。
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Catalog {
    pub fn new(
        account: Account,
        cache_path: &Path,
        benefit_fallback: bool,
        http: reqwest::Client,
    ) -> Self {
        let mut models = BTreeMap::new();
        for id in FALLBACK_AGENT_MODELS {
            models.insert(id.to_string(), ModelInfo::new(id.to_string(), "agent"));
        }
        if benefit_fallback {
            for id in BENEFIT_MODELS {
                models.insert(id.to_string(), ModelInfo::new(id.to_string(), "benefit"));
            }
        }
        Self {
            id: account.id.clone(),
            account: tokio::sync::RwLock::new(account),
            inner: Arc::new(tokio::sync::RwLock::new(CatalogState {
                models,
                last_refresh: None,
            })),
            cache_path: cache_path.to_path_buf(),
            benefit_fallback,
            http,
            last_refresh_creds: std::sync::Mutex::new(None),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// 当前凭证的快照。
    pub async fn account(&self) -> Account {
        self.account.read().await.clone()
    }

    /// 在发请求前确保证件可用。
    ///
    /// OAuth 账号的 STS 临时凭证有效期 24 小时，但实测可能提前失效
    /// （`APIG.0602 security token has expired`）。这里在剩余有效期不足
    /// 5 分钟时主动续期，并在续期后替换内存中的凭证。
    ///
    /// 用 `last_refresh_creds` 做 60 秒节流：并发请求同时撞到过期时，
    /// 只有一个会真的去换 token，其余直接复用结果。
    pub async fn ensure_credentials(&self, state: &crate::web::WebState) -> Result<(), String> {
        let current = self.account().await;
        if !current.is_oauth() {
            return Ok(());
        }

        // OAuth 身份只在凭证检查或续期时读取；未过期时不访问磁盘。
        let env_dir = &state.config().env_dir;
        let identity = match crate::oauth::store::load(env_dir, self.account_id()) {
            Some(id) => id,
            None => return Ok(()), // 没有 auths 文件时无能为力，让上游报错
        };

        // 还剩 5 分钟以上就不折腾
        if !identity.expired() && !identity.expiring_within(300) {
            return Ok(());
        }

        {
            let guard = self
                .last_refresh_creds
                .lock()
                .map_err(|_| "凭证锁中毒".to_string())?;
            if let Some(t) = *guard {
                if t.elapsed() < Duration::from_secs(60) {
                    return Ok(()); // 刚续过，不重复续
                }
            }
        }

        let fresh = crate::oauth::store::refresh_and_save(&self.http, env_dir, &identity).await?;
        {
            let mut acct = self.account.write().await;
            acct.ak = fresh.access_key_id.clone();
            acct.sk = fresh.secret_access_key.clone();
            acct.security_token = Some(fresh.security_token.clone());
        }
        if let Ok(mut guard) = self.last_refresh_creds.lock() {
            *guard = Some(std::time::Instant::now());
        }
        println!("[{}] 凭证已自动续期", self.account_id());
        Ok(())
    }

    pub fn account_id(&self) -> &str {
        &self.id
    }

    pub fn masked_ak(&self) -> String {
        self.account
            .try_read()
            .map(|a| a.masked_ak())
            .unwrap_or_else(|_| "****".to_string())
    }

    async fn load_cache_data(&self) -> Option<CacheFile> {
        let text = tokio::fs::read_to_string(&self.cache_path).await.ok()?;
        serde_json::from_str(&text).ok()
    }

    async fn load_cache(&self) {
        let cache = {
            let _guard = self.refresh_lock.lock().await;
            self.load_cache_data().await
        };
        if let Some(cache) = cache {
            self.install_cache(cache).await;
        }
    }

    async fn install_cache(&self, cache: CacheFile) {
        if cache.models.is_empty() {
            return;
        }
        let _guard = self.refresh_lock.lock().await;
        let cached_at = tokio::fs::metadata(&self.cache_path)
            .await
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .filter(|age| *age < CACHE_TTL)
            .map(|_| std::time::Instant::now());
        let models = cache.models;
        // 缓存保留目录同步时的真实能力；新版本会追加已注册的视觉模型。
        let mut state = self.inner.write().await;
        state.models = models.into_iter().map(|m| (m.id.clone(), m)).collect();
        if cached_at.is_some() {
            state.last_refresh = cached_at;
        }
    }

    async fn save_cache(&self, models: &[ModelInfo]) {
        let payload = json!({
            "updated_at": chrono::Utc::now().timestamp(),
            "account": self.id,
            "models": models,
        });
        let Ok(text) = serde_json::to_string_pretty(&payload) else {
            return;
        };
        if let Some(dir) = self.cache_path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let tmp = self.cache_path.with_extension("json.tmp");
        if tokio::fs::write(&tmp, text).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &self.cache_path).await;
        }
    }

    async fn signed_send(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Result<(u16, String), reqwest::Error> {
        let account = self.account().await;
        let signer = Signer::new(&account.ak, &account.sk);
        // OAuth 账号用 STS 临时凭证，必须走 sign_full（带 X-Sdk-Content-Sha256
        // 与 X-Security-Token）；手动 AK/SK 账号保持原有基础签名。
        let signed = if account.is_oauth() {
            signer.sign_full(method, url, body, extra, account.security_token())
        } else {
            signer.sign(method, url, body, extra)
        };
        let mut req = self
            .http
            .request(method.parse().unwrap_or(reqwest::Method::GET), url);
        for (k, v) in signed.headers {
            req = req.header(k, v);
        }
        if !body.is_empty() {
            req = req.body(body.to_vec());
        }
        let resp = req.timeout(Duration::from_secs(20)).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        Ok((status, text))
    }

    pub async fn sign_headers(
        &self,
        body: &[u8],
        benefit: bool,
        model_id: &str,
    ) -> Vec<(String, String)> {
        let account = self.account().await;

        // 会话标识必须让上游看到"同一个会话"，否则每个请求都会被当成新会话，
        // 连续 3 个之后必被 `TM.00001041` 拒绝（详见 concurrency::account_session_id）。
        // 这两个头**必须参与签名**，所以放进 extra 而不是签名后追加。
        let session_id = crate::concurrency::account_session_id(&account.session_seed());

        let mut extra: Vec<(&str, &str)> = vec![
            ("x-ot-session-id", session_id.as_str()),
            ("user-session-id", session_id.as_str()),
        ];
        // model-id / model-name 对所有模型都带（实测非 benefit 模型也接受），
        // 必须与请求体里的 model 完全一致。maas_type 仍然只给 benefit。
        if !model_id.is_empty() {
            extra.push(("model-id", model_id));
            extra.push(("model-name", model_id));
        }
        if benefit {
            extra.push(("maas_type", "benefit"));
        }

        let signer = Signer::new(&account.ak, &account.sk);
        if account.is_oauth() {
            signer
                .sign_full("POST", TARGET, body, &extra, account.security_token())
                .headers
        } else {
            signer.sign("POST", TARGET, body, &extra).headers
        }
    }

    /// 为附件等其他 CodeArts 端点签名，复用当前账号凭证与续期语义。
    pub async fn signed_request(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
        extra: &[(&str, &str)],
    ) -> Vec<(String, String)> {
        let account = self.account().await;
        let signer = Signer::new(&account.ak, &account.sk);
        if account.is_oauth() {
            signer
                .sign_full(method, url, body, extra, account.security_token())
                .headers
        } else {
            signer.sign(method, url, body, extra).headers
        }
    }

    pub fn http_client(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// 从 opengw 免费额度网关取模型列表；失败返回空。
    async fn refresh_benefit_models(&self) -> Vec<ModelInfo> {
        match self
            .signed_send("GET", OPENGW_CONFIG_URL, b"", &[("x-language", "zh-cn")])
            .await
        {
            Ok((200, text)) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    return Vec::new();
                };
                let Some(models) = value
                    .get("result")
                    .and_then(|r| r.get("models"))
                    .and_then(|m| m.as_array())
                else {
                    return Vec::new();
                };
                let mut out: Vec<ModelInfo> = Vec::new();
                for item in models {
                    let Some(model_id) = item.get("model_id").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if out.iter().any(|m| m.id == model_id) {
                        continue;
                    }
                    let mut info = ModelInfo::new(model_id.to_string(), "benefit");
                    info.name = item
                        .get("model_name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    info.description = Some("免费额度模型（签到领取）".to_string());
                    info.context_window = item.get("context_window").and_then(|v| v.as_i64());
                    info.max_tokens = item
                        .get("max_tokens")
                        .and_then(|v| v.as_i64())
                        .map(|v| v.max(crate::params::UPSTREAM_MAX_OUTPUT_TOKENS));
                    out.push(info);
                }
                out
            }
            Ok((status, text)) => {
                eprintln!(
                    "[{}] 免费模型列表同步失败（HTTP {status}）: {}",
                    self.id,
                    text.chars().take(200).collect::<String>()
                );
                Vec::new()
            }
            Err(e) => {
                eprintln!("[{}] 免费模型列表同步失败: {e}", self.id);
                Vec::new()
            }
        }
    }

    /// PromptCenter 的模型目录提供实际注册的图片理解能力。
    async fn refresh_builtin_models(&self) -> Vec<ModelInfo> {
        let extra = [
            ("agent-type", "PromptCenter"),
            ("x-language", "zh-cn"),
            ("accept", "application/json"),
        ];
        let Ok((200, text)) = self
            .signed_send("GET", MODEL_BUILTIN_URL, b"", &extra)
            .await
        else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        builtin_model_values(&value)
            .into_iter()
            .filter_map(|model| {
                let id = model.get("model_id")?.as_str()?;
                let mut info = ModelInfo::new(id.to_string(), "agent");
                info.name = model
                    .get("model_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                info.description = model
                    .get("model_desc")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                info.supports_images = model
                    .get("supports_images")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Some(info)
            })
            .collect()
    }

    /// 从 AgentCenter 同步真实模型列表。成功返回 true。
    pub async fn refresh(&self) -> bool {
        let _guard = self.refresh_lock.lock().await;
        self.refresh_locked().await
    }

    async fn refresh_locked(&self) -> bool {
        let extra = [
            ("agent-type", "AgentCenter"),
            ("x-language", "zh-cn"),
            ("accept", "application/json"),
        ];
        let (status, text) = match self.signed_send("GET", AGENT_LIST_URL, b"", &extra).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[{}] 自动同步模型失败: {e}", self.id);
                return false;
            }
        };
        if status != 200 {
            eprintln!("[{}] 自动同步模型失败（HTTP {status}）", self.id);
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            eprintln!("[{}] 自动同步模型失败：响应不是合法 JSON", self.id);
            return false;
        };
        let Some(agents) = value.get("agents").and_then(|v| v.as_array()) else {
            return false;
        };
        if agents.is_empty() {
            return false;
        }

        let mut sorted: Vec<&Value> = agents.iter().collect();
        sorted.sort_by_key(|a| {
            let primary = a
                .get("is_primary_agent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let order = a.get("agent_order").and_then(|v| v.as_i64());
            (!primary, order.is_none(), order.unwrap_or(999_999))
        });

        let mut discovered: Vec<ModelInfo> = Vec::new();
        for agent in sorted {
            let Some(agent_id) = agent.get("agent_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let detail_url = format!(
                "{BASE_URL}{AGENT_DETAIL_PATH}?agent_id={}",
                crate::signer::encode_component(agent_id)
            );
            let (dstatus, dtext) = match self.signed_send("GET", &detail_url, b"", &extra).await {
                Ok(v) => v,
                Err(_) => continue,
            };
            if dstatus != 200 {
                continue;
            }
            let Ok(detail) = serde_json::from_str::<Value>(&dtext) else {
                continue;
            };
            let Some(models) = detail
                .get("gpts")
                .and_then(|g| g.get("models"))
                .and_then(|m| m.as_array())
            else {
                continue;
            };
            for model in models {
                let model_id = model
                    .get("model_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| model.get("model_alias").and_then(|v| v.as_str()));
                let Some(model_id) = model_id else { continue };
                if discovered.iter().any(|m| m.id == model_id) {
                    continue;
                }
                let params = model.get("model_parameters");
                let mut info = ModelInfo::new(model_id.to_string(), "agent");
                info.name = model
                    .get("model_alias")
                    .and_then(|v| v.as_str())
                    .or_else(|| model.get("model_name").and_then(|v| v.as_str()))
                    .map(|s| s.to_string());
                info.description = model
                    .get("model_desc")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                info.context_window = params
                    .and_then(|p| p.get("context_window"))
                    .and_then(|v| v.as_i64());
                info.max_tokens = params
                    .and_then(|p| p.get("max_tokens"))
                    .and_then(|v| v.as_i64())
                    // 上游 model_parameters.max_tokens 恒为 65536，但真实校验器
                    // 允许到 131072。以校验器为准，见 params::UPSTREAM_MAX_OUTPUT_TOKENS。
                    .map(|v| v.max(crate::params::UPSTREAM_MAX_OUTPUT_TOKENS));
                info.supports_images = params
                    .and_then(|p| p.get("supports_images"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| supports_images(model_id));
                discovered.push(info);
            }
            if !discovered.is_empty() {
                break;
            }
        }

        if discovered.is_empty() {
            return false;
        }

        // AgentCenter 只列文本模型；PromptCenter 还包含已注册的图片理解模型。
        for info in self.refresh_builtin_models().await {
            if let Some(existing) = discovered.iter_mut().find(|m| m.id == info.id) {
                existing.supports_images = info.supports_images;
            } else if info.supports_images {
                discovered.push(info);
            }
        }

        // 免费模型不在 AgentCenter 列表里，从网关补；拿不到时按配置决定是否兜底。
        let benefit = self.refresh_benefit_models().await;
        if benefit.is_empty() && self.benefit_fallback {
            for id in BENEFIT_MODELS {
                discovered.push(ModelInfo::new(id.to_string(), "benefit"));
            }
        } else {
            for info in benefit {
                if !discovered.iter().any(|m| m.id == info.id) {
                    discovered.push(info);
                }
            }
        }

        let ids: Vec<String> = discovered.iter().map(|m| m.id.clone()).collect();
        {
            let mut state = self.inner.write().await;
            state.models = discovered
                .iter()
                .cloned()
                .map(|m| (m.id.clone(), m))
                .collect();
            state.last_refresh = Some(std::time::Instant::now());
        }
        self.save_cache(&discovered).await;
        println!(
            "[{}] 已同步 {} 个模型: {}",
            self.id,
            ids.len(),
            ids.join(", ")
        );
        true
    }

    /// 过期目录仍可服务请求；后台单飞刷新，不让控制接口拖住聊天首字。
    pub fn ensure_fresh(self: &Arc<Self>) {
        if self.is_fresh() {
            return;
        }
        let Ok(guard) = self.refresh_lock.clone().try_lock_owned() else {
            return;
        };
        // 拿到刷新锁即表示可以启动后台任务。
        // 等待锁期间另一个刷新可能已经完成。
        if self.is_fresh() {
            return;
        }
        let catalog = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            if !catalog.refresh_locked().await {
                // 当前内存快照可以继续服务；不重新安装可能已过期的磁盘缓存。
                catalog.inner.write().await.last_refresh = Some(std::time::Instant::now());
            }
        });
    }

    fn is_fresh(&self) -> bool {
        self.inner
            .try_read()
            .ok()
            .and_then(|state| state.last_refresh)
            .map(|t| t.elapsed() <= CACHE_TTL)
            .unwrap_or(false)
    }

    pub async fn models(&self) -> Vec<ModelInfo> {
        self.inner.read().await.models.values().cloned().collect()
    }

    pub async fn contains(&self, id: &str) -> bool {
        self.inner.read().await.models.contains_key(id)
    }

    pub async fn model(&self, id: &str) -> Option<ModelInfo> {
        self.inner.read().await.models.get(id).cloned()
    }

    pub async fn supports_images(&self, model_id: &str) -> bool {
        self.inner
            .read()
            .await
            .models
            .get(model_id)
            .map(|m| m.supports_images)
            .unwrap_or(false)
    }

    pub async fn is_agent_channel(&self, id: &str) -> bool {
        self.inner
            .read()
            .await
            .models
            .get(id)
            .map(|m| m.channel == "agent")
            .unwrap_or(false)
    }
}

/// 多账号池：合并模型列表并按顺序提供故障切换。
#[derive(Clone)]
pub struct Pool {
    catalogs: Vec<Arc<Catalog>>,
}

impl Pool {
    pub fn new(catalogs: Vec<Arc<Catalog>>) -> Self {
        Self { catalogs }
    }

    /// 缓存优先构建账号池；CLI 可选择等待同步完成。
    pub async fn build(
        cfg: &crate::config::Config,
        http: reqwest::Client,
        wait_for_refresh: bool,
    ) -> Result<Self, String> {
        let mut catalogs = Vec::new();
        let mut placeholders = Vec::new();
        for account in cfg.accounts.iter().filter(|a| a.enabled) {
            if account.looks_like_placeholder() {
                // 汇总到最后的错误里统一提示，这里只登记，不重复刷屏。
                placeholders.push(account.id.clone());
                continue;
            }
            let resolved = match crate::admin::resolve_account(&http, &cfg.env_dir, account).await {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("[pool] 跳过账号 {}: {e}", account.id);
                    continue;
                }
            };
            let catalog = Arc::new(Catalog::new(
                resolved,
                &cfg.cache_path_for(account),
                cfg.benefit_fallback,
                http.clone(),
            ));
            if wait_for_refresh {
                if !catalog.refresh().await {
                    catalog.load_cache().await;
                }
            } else {
                catalog.load_cache().await;
                catalog.ensure_fresh();
            }
            catalogs.push(catalog);
        }
        if catalogs.is_empty() {
            // 占位符账号是「还没配置真实凭证」，和凭证失效不是一回事。
            if !placeholders.is_empty()
                && placeholders.len() == cfg.accounts.iter().filter(|a| a.enabled).count()
            {
                return Err(format!(
                    "账号 {} 的 AK/SK 还是示例占位符：请填入真实密钥，\
                     或启动 serve 后在 WebUI 点「登录华为云」",
                    placeholders.join(", ")
                ));
            }
            // 零账号是首次使用的正常状态（等着从 WebUI 走 OAuth），
            // 但"配了账号却一个都初始化不了"是真错误。
            if cfg.accounts.iter().any(|a| a.enabled) {
                return Err("没有任何账号能成功初始化（OAuth 账号可能需要重新登录）".to_string());
            }
            return Ok(Self::new(Vec::new()));
        }
        Ok(Self::new(catalogs))
    }

    pub fn catalogs(&self) -> &[Arc<Catalog>] {
        &self.catalogs
    }

    pub fn ensure_fresh(&self) {
        for catalog in &self.catalogs {
            catalog.ensure_fresh();
        }
    }

    /// 合并所有账号的模型，同 id 取第一个账号的元数据（信息更完整者优先）。
    pub async fn merged_models(&self) -> Vec<ModelInfo> {
        let mut merged: BTreeMap<String, ModelInfo> = BTreeMap::new();
        for catalog in &self.catalogs {
            for model in catalog.models().await {
                match merged.get(&model.id) {
                    Some(existing) => {
                        let mut better = existing.clone();
                        if better.context_window.is_none() {
                            better.context_window = model.context_window;
                        }
                        if better.max_tokens.is_none() {
                            better.max_tokens = model.max_tokens;
                        }
                        if better.description.as_deref().unwrap_or("").is_empty() {
                            better.description = model.description.clone();
                        }
                        better.supports_images |= model.supports_images;
                        merged.insert(better.id.clone(), better);
                    }
                    None => {
                        merged.insert(model.id.clone(), model);
                    }
                }
            }
        }
        merged.into_values().collect()
    }

    /// 该模型在哪些账号上可用（保持配置顺序，用于故障切换）。
    pub async fn accounts_for(&self, model_id: &str) -> Vec<Arc<Catalog>> {
        let mut out = Vec::new();
        for catalog in &self.catalogs {
            if catalog.contains(model_id).await {
                out.push(catalog.clone());
            }
        }
        out
    }

    /// 首次图片请求可能先于后台目录刷新到达。给正在进行的同步一个短暂
    /// 完成窗口；未过期目录不等待，也不为每个请求重复发起刷新。
    pub async fn wait_for_model(&self, model_id: &str) -> bool {
        const REFRESH_WAIT: Duration = Duration::from_secs(8);
        let deadline = std::time::Instant::now() + REFRESH_WAIT;
        loop {
            if self.contains(model_id).await {
                return true;
            }
            let stale = self.catalogs.iter().any(|catalog| !catalog.is_fresh());
            if !stale || std::time::Instant::now() >= deadline {
                return false;
            }
            for catalog in &self.catalogs {
                catalog.ensure_fresh();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn contains(&self, model_id: &str) -> bool {
        for catalog in &self.catalogs {
            if catalog.contains(model_id).await {
                return true;
            }
        }
        false
    }

    /// 模型的输出上限：取第一个可用账号的元数据，缺失则由调用方兜底。
    pub async fn max_tokens_of(&self, model_id: &str) -> Option<i64> {
        for catalog in &self.catalogs {
            if let Some(info) = catalog.model(model_id).await {
                if let Some(v) = info.max_tokens {
                    return Some(v);
                }
            }
        }
        None
    }

    pub async fn supports_images(&self, model_id: &str) -> bool {
        for catalog in &self.catalogs {
            if catalog.contains(model_id).await {
                return catalog.supports_images(model_id).await;
            }
        }
        false
    }

    pub async fn first_model(&self) -> Option<String> {
        for catalog in &self.catalogs {
            if let Some(id) = catalog.models().await.first().map(|m| m.id.clone()) {
                return Some(id);
            }
        }
        None
    }

    /// 汇总每个账号的模型数量，供 accounts 子命令展示。
    pub async fn describe(&self) -> Vec<(String, String, usize)> {
        let mut out = Vec::new();
        for catalog in &self.catalogs {
            out.push((
                catalog.account_id().to_string(),
                catalog.masked_ak(),
                catalog.models().await.len(),
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_vision_models_are_not_mislabeled() {
        for id in ["kimi-k2.6-vl", "Qwen3-VL-235B"] {
            assert!(ModelInfo::new(id.to_string(), "agent").supports_images);
        }
        for id in [
            "kimi-k3.1-pro",
            "deepseek-v4.1-flash",
            "deepseek-v4.1-flash-1017",
        ] {
            assert!(!ModelInfo::new(id.to_string(), "agent").supports_images);
        }
    }

    #[test]
    fn parses_builtin_model_shapes() {
        let value = json!({"builtinModels": [{"model_id": "kimi-k2.6-vl"}]});
        assert_eq!(builtin_model_values(&value).len(), 1);
    }

    #[tokio::test]
    async fn fresh_cache_replaces_builtin_fallback_models() {
        let dir = std::env::temp_dir().join(format!("ca2a-cache-test-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("models.json");
        let cache = CacheFile {
            updated_at: chrono::Utc::now().timestamp(),
            models: vec![ModelInfo::new("kimi-k2.6-vl".to_string(), "agent")],
        };
        tokio::fs::write(&path, serde_json::to_vec(&cache).unwrap())
            .await
            .unwrap();
        let catalog = Catalog::new(
            Account::manual("test".into(), "ak".into(), "sk".into(), true),
            &path,
            false,
            reqwest::Client::new(),
        );
        catalog.load_cache().await;
        assert!(catalog.contains("kimi-k2.6-vl").await);
        assert!(!catalog.contains(FALLBACK_AGENT_MODELS[0]).await);
        assert!(catalog.is_fresh());
        assert!(catalog.model("kimi-k2.6-vl").await.unwrap().supports_images);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
