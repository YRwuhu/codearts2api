//! 模型目录：云端同步 + 本地缓存 + 通道判定。
//!
//! 模型分两条上游通道：
//! - `agent`：老套餐模型，走 AgentCenter；
//! - `benefit`：免费额度模型，签名时必须多带 maas_type / model-id / model-name。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

#[derive(Clone)]
pub struct Catalog {
    inner: Arc<tokio::sync::RwLock<CatalogState>>,
    signer: Arc<SignerOwned>,
    cache_path: std::path::PathBuf,
    benefit_fallback: bool,
    http: reqwest::Client,
}

struct SignerOwned {
    ak: String,
    sk: String,
}

struct CatalogState {
    models: BTreeMap<String, ModelInfo>,
    last_refresh: Option<std::time::Instant>,
}

impl Catalog {
    pub fn new(
        ak: &str,
        sk: &str,
        cache_path: &Path,
        benefit_fallback: bool,
        http: reqwest::Client,
    ) -> Self {
        let signer = Arc::new(SignerOwned {
            ak: ak.to_string(),
            sk: sk.to_string(),
        });
        let mut models = BTreeMap::new();
        if !benefit_fallback {
            for id in FALLBACK_AGENT_MODELS {
                models.insert(id.to_string(), ModelInfo::new(id.to_string(), "agent"));
            }
        } else {
            for id in FALLBACK_AGENT_MODELS {
                models.insert(id.to_string(), ModelInfo::new(id.to_string(), "agent"));
            }
            for id in BENEFIT_MODELS {
                models.insert(id.to_string(), ModelInfo::new(id.to_string(), "benefit"));
            }
        }
        Self {
            inner: Arc::new(tokio::sync::RwLock::new(CatalogState {
                models,
                last_refresh: None,
            })),
            signer,
            cache_path: cache_path.to_path_buf(),
            benefit_fallback,
            http,
        }
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
        let signer = Signer::new(&self.signer.ak, &self.signer.sk);
        let signed = signer.sign(method, url, body, extra);
        let mut req = self.http.request(
            method.parse().unwrap_or(reqwest::Method::GET),
            url,
        );
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
        let signer = Signer::new(&self.signer.ak, &self.signer.sk);
        signer.sign("POST", TARGET, body, &extra).headers
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
                    info.max_tokens = item.get("max_tokens").and_then(|v| v.as_i64());
                    out.push(info);
                }
                out
            }
            Ok((status, text)) => {
                eprintln!("免费模型列表同步失败（HTTP {status}）: {}", &text[..text.len().min(200)]);
                Vec::new()
            }
            Err(e) => {
                eprintln!("免费模型列表同步失败: {e}");
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
                eprintln!("自动同步模型失败，将使用缓存或兜底列表: {e}");
                return false;
            }
        };
        if status != 200 {
            eprintln!("自动同步模型失败（HTTP {status}）");
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            eprintln!("自动同步模型失败：响应不是合法 JSON");
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
            let (dstatus, dtext) = match self
                .signed_send("GET", &detail_url, b"", &extra)
                .await
            {
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
                info.max_tokens = params.and_then(|p| p.get("max_tokens")).and_then(|v| v.as_i64());
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
        println!("已自动同步模型: {}", ids.join(", "));
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

    pub async fn is_agent_channel(&self, id: &str) -> bool {
        self.inner
            .read()
            .await
            .models
            .get(id)
            .map(|m| m.channel == "agent")
            .unwrap_or(false)
    }

    pub async fn max_tokens_of(&self, id: &str) -> Option<i64> {
        self.inner
            .read()
            .await
            .models
            .get(id)
            .and_then(|m| m.max_tokens)
    }

    pub async fn first_model(&self) -> Option<String> {
        self.inner.read().await.models.keys().next().cloned()
    }
}
