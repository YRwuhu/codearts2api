//! WebUI 静态资源与共享状态。
//!
//! 前端资源在编译期嵌入二进制（include_dir），因此单个 exe 即可完整运行，
//! 不依赖部署目录里的 static/ 文件夹。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use include_dir::{include_dir, Dir};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::models::Pool;
use crate::quota::Quota;
use crate::usage::UsageStore;

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

#[derive(Clone)]
pub struct WebState {
    config: Arc<std::sync::RwLock<Config>>,
    pub pool: Arc<RwLock<Pool>>,
    pub http: reqwest::Client,
    pub usage: Arc<UsageStore>,
    pub log: crate::logging::Logger,
    /// 最近一次额度查询结果（按账号缓存），由定时任务或 WebUI 触发更新。
    pub quota: Arc<RwLock<HashMap<String, Quota>>>,
    /// 每个账号最近一次签到结果，按账号归属并持久化到 checkin.json。
    pub checkins: Arc<RwLock<crate::checkin::History>>,
    /// 同一账号的手动与后台签到互斥。
    pub checkin_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// 每账号的并发闸门与健康状态。
    pub gates: Arc<RwLock<HashMap<String, Arc<crate::concurrency::AccountGate>>>>,
    /// 会话钉号表。
    pub pins: crate::concurrency::SessionPins,
}

impl WebState {
    pub fn new(config: Config, pool: Pool, http: reqwest::Client, usage: Arc<UsageStore>) -> Self {
        let checkins = crate::checkin::load_history(&config.env_dir);
        let log = crate::logging::Logger::new(Some(config.env_dir.join("logs/server.log")));
        Self {
            config: Arc::new(std::sync::RwLock::new(config)),
            pool: Arc::new(RwLock::new(pool)),
            http,
            usage,
            log,
            quota: Arc::new(RwLock::new(HashMap::new())),
            checkins: Arc::new(RwLock::new(checkins)),
            checkin_locks: Arc::new(RwLock::new(HashMap::new())),
            gates: Arc::new(RwLock::new(HashMap::new())),
            pins: crate::concurrency::SessionPins::new(),
        }
    }

    /// 取配置快照，避免长时间持锁。
    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 取（或按需创建）某账号的并发闸门。
    pub async fn gate(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Arc<crate::concurrency::AccountGate> {
        {
            let guard = self.gates.read().await;
            if let Some(g) = guard.get(account_id) {
                return g.clone();
            }
        }
        let mut guard = self.gates.write().await;
        guard
            .entry(account_id.to_string())
            .or_insert_with(|| Arc::new(crate::concurrency::AccountGate::new(limit)))
            .clone()
    }

    pub async fn checkin_lock(&self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(lock) = self.checkin_locks.read().await.get(account) {
            return lock.clone();
        }
        self.checkin_locks
            .write()
            .await
            .entry(account.to_string())
            .or_default()
            .clone()
    }

    pub fn reload_config(&self) -> Result<(), String> {
        let current = self.config();
        let fresh = if current.explicit_data_dir {
            let mut config = Config::load_from_dir(&current.env_dir)?;
            // The live listener and OAuth callback use the active address until restart.
            config.host = current.host;
            config.port = current.port;
            config
        } else {
            Config::load()?
        };
        *self
            .config
            .write()
            .unwrap_or_else(|error| error.into_inner()) = fresh;
        Ok(())
    }
}

fn guess_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// 静态资源与 SPA 入口：未匹配的路径一律回落到 index.html。
pub async fn static_handler(State(state): State<WebState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if path != "index.html" {
        if let Some(file) = ASSETS.get_file(path) {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, guess_mime(path))],
                file.contents(),
            )
                .into_response();
        }
    }

    // 根路径与前端路由（/accounts、/models）都交给 SPA 处理
    match ASSETS.get_file("index.html") {
        Some(index) => {
            let html = index
                .contents_utf8()
                .expect("embedded index.html uses UTF-8");
            let html = if state.config().explicit_data_dir {
                std::borrow::Cow::Owned(html.replacen(
                    "data-platform=\"desktop\"",
                    "data-platform=\"android\"",
                    1,
                ))
            } else {
                std::borrow::Cow::Borrowed(html)
            };
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
        }
        None => (StatusCode::NOT_FOUND, "index.html 未嵌入").into_response(),
    }
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/", get(static_handler))
        .route("/accounts", get(static_handler))
        .route("/models", get(static_handler))
        .route("/quota", get(static_handler))
        .route("/usage", get(static_handler))
        .route("/checkin", get(static_handler))
        .route("/oauth/callback", get(crate::oauth::flow::callback))
        .fallback(static_handler)
}
