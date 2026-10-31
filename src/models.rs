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
pub const OPENGW_CONFIG_URL: &str = "https://opengw.developer.huaweicloud.com/api/v1/gateway/config";

pub const FALLBACK_AGENT_MODELS: [&str; 3] = ["openpangu-2.0-pro", "openpangu-2.0-flash", "GLM-5.2"];
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
}

fn default_object() -> String {
    "model".to_string()
}
fn default_channel() -> String {
    "agent".to_string()
}

impl ModelInfo {
    fn new(id: String, channel: &str) -> Self {
        Self {
            name: Some(id.clone()),
            id,
            object: default_object(),
            owned_by: "codearts".to_string(),
            channel: channel.to_string(),
            description: None,
            context_window: None,
            max_tokens: None,
            supports_images: false,
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
    /// 连续失败次数，用于 fetch 失败时回退缓存并抑制日志噪声。
    consecutive_failures: Arc<std::sync::atomic::AtomicU32>,
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
            consecutive_failures: Arc::new(std::sync::atomic::AtomicU32::new(0)),
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

        let env_dir = state.config().env_dir;
        let identity = match crate::oauth::store::load(&env_dir, &self.account_id().to_string()) {
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

        let fresh = crate::oauth::store::refresh_and_save(&self.http, &env_dir, &identity).await?;
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

    pub async fn load_cache(&self) {
        let Ok(text) = tokio::fs::read_to_string(&self.cache_path).await else {
            return;
        };
        let Ok(cache) = serde_json::from_str::<CacheFile>(&text) else {
            return;
        };
        if cache.models.is_empty() {
            return;
        }
        let mut state = self.inner.write().await;
        state.models = cache.models.into_iter().map(|m| (m.id.clone(), m)).collect();
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
        let resp = req.send().await?;
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
        let mut extra: Vec<(&str, &str)> = Vec::new();
        if benefit && !model_id.is_empty() {
            extra.push(("maas_type", "benefit"));
            extra.push(("model-id", model_id));
            extra.push(("model-name", model_id));
        }
        let account = self.account().await;
        let signer = Signer::new(&account.ak, &account.sk);
        if account.is_oauth() {
            signer
                .sign_full("POST", TARGET, body, &extra, account.security_token())
                .headers
        } else {
            signer.sign("POST", TARGET, body, &extra).headers
        }
    }


    /// 从 opengw 免费额度网关取模型列表；失败返回空。
    async fn refresh_benefit_models(&self) -> Vec<ModelInfo> {
        match self
            .signed_send("GET", OPENGW_CONFIG_URL, b"", &[("x-language", "zh-cn")])
            .await
        {
            Ok((status, text)) if status == 200 => {
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
                    &text[..text.len().min(200)]
                );
                Vec::new()
            }
            Err(e) => {
                eprintln!("[{}] 免费模型列表同步失败: {e}", self.id);
                Vec::new()
            }
        }
    }

    /// 从 AgentCenter 同步真实模型列表。成功返回 true。
    pub async fn refresh(&self) -> bool {
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
                    .unwrap_or(false);
                discovered.push(info);
            }
            if !discovered.is_empty() {
                break;
            }
        }

        if discovered.is_empty() {
            return false;
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
        println!("[{}] 已同步 {} 个模型: {}", self.id, ids.len(), ids.join(", "));
        true
    }

    /// 短周期缓存，避免客户端轮询 /v1/models 时反复请求上游。
    pub async fn ensure_fresh(&self, force_initial: bool) {
        let needs = {
            let state = self.inner.read().await;
            if force_initial {
                true
            } else {
                match state.last_refresh {
                    Some(t) => t.elapsed() > CACHE_TTL,
                    None => true,
                }
            }
        };
        if !needs {
            return;
        }

        // 缓存文件较新时优先用缓存，避免每次重启都打上游
        if let Ok(meta) = tokio::fs::metadata(&self.cache_path).await {
            if let Ok(modified) = meta.modified() {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .unwrap_or(Duration::from_secs(u64::MAX / 2));
                if age < CACHE_TTL {
                    let empty = self.inner.read().await.models.is_empty();
                    if empty {
                        self.load_cache().await;
                    }
                    let mut state = self.inner.write().await;
                    state.last_refresh = Some(std::time::Instant::now());
                    return;
                }
            }
        }

        if !self.refresh().await {
            self.load_cache().await;
            let mut state = self.inner.write().await;
            state.last_refresh = Some(std::time::Instant::now());
        }
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

    pub async fn is_agent_channel(&self, id: &str) -> bool {
        self.inner
            .read()
            .await
            .models
            .get(id)
            .map(|m| m.channel == "agent")
            .unwrap_or(false)
    }

    pub fn note_failure(&self) {
        self.consecutive_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn note_success(&self) {
        self.consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
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

    pub fn catalogs(&self) -> &[Arc<Catalog>] {
        &self.catalogs
    }

    pub async fn ensure_fresh(&self) {
        for catalog in &self.catalogs {
            catalog.ensure_fresh(false).await;
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
