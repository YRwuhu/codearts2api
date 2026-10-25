//! WebUI 静态资源与共享状态。
//!
//! 前端资源在编译期嵌入二进制（include_dir），因此单个 exe 即可完整运行，
//! 不依赖部署目录里的 static/ 文件夹。

use std::sync::Arc;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use include_dir::{include_dir, Dir};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::models::Pool;

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

#[derive(Clone)]
pub struct WebState {
    pub config: Arc<RwLock<Config>>,
    pub pool: Arc<RwLock<Pool>>,
    pub http: reqwest::Client,
}

impl WebState {
    pub fn new(config: Config, pool: Pool, http: reqwest::Client) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            pool: Arc::new(RwLock::new(pool)),
            http,
        }
    }

    /// 取配置快照，避免长时间持锁。
    pub fn config(&self) -> Config {
        self.config
            .try_read()
            .map(|c| c.clone())
            .unwrap_or_else(|_| Config::load().expect("配置在运行期不应失效"))
    }

    pub fn reload_config(&self) -> Result<(), String> {
        let fresh = Config::load()?;
        if let Ok(mut guard) = self.config.try_write() {
            *guard = fresh;
            Ok(())
        } else {
            Err("配置正被占用，请重试".to_string())
        }
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
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = ASSETS.get_file(path) {
        let mime = guess_mime(path);
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime)],
            file.contents().to_vec(),
        )
            .into_response();
    }

    // 根路径与前端路由（/accounts、/models）都交给 SPA 处理
    match ASSETS.get_file("index.html") {
        Some(index) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            index.contents().to_vec(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "index.html 未嵌入").into_response(),
    }
}

pub fn router() -> Router<WebState> {
    Router::new()
        .route("/", get(static_handler))
        .route("/accounts", get(static_handler))
        .route("/models", get(static_handler))
        .fallback(static_handler)
}
