//! Desktop access control. Loopback-only setups retain the zero-config local UI;
//! a non-loopback listener always requires a separate proxy key and admin login.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::web::WebState;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub lan: Option<bool>,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub admin_salt: String,
    #[serde(default)]
    pub admin_hash: String,
}

impl Settings {
    pub fn protected(&self) -> bool {
        self.lan == Some(true) || !self.api_key.is_empty() || !self.admin_hash.is_empty()
    }
}

pub fn load(dir: &Path) -> Settings {
    std::fs::read_to_string(path(dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn path(dir: &Path) -> PathBuf {
    dir.join("access.json")
}

fn save(dir: &Path, settings: &Settings) -> Result<(), String> {
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let temp = dir.join("access.json.tmp");
    std::fs::write(&temp, text).map_err(|e| format!("写入访问配置失败: {e}"))?;
    std::fs::rename(temp, path(dir)).map_err(|e| format!("保存访问配置失败: {e}"))
}

#[cfg(test)]
pub(crate) fn save_for_test(dir: &Path, settings: &Settings) {
    save(dir, settings).unwrap();
}

fn secret() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn password_hash(password: &str, salt: &str) -> String {
    let mut output = [0u8; 32];
    let rounds = if cfg!(test) { 1 } else { 120_000 };
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt.as_bytes(), rounds, &mut output);
    format!("pbkdf2-sha256:{rounds}:{}", hex::encode(output))
}

fn equal_secret(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

#[derive(Default)]
struct LoginAttempts {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Clone)]
pub struct Access {
    settings: Arc<std::sync::RwLock<Settings>>,
    env_dir: PathBuf,
    listen: String,
    requested_lan: bool,
    logins: Arc<Mutex<LoginAttempts>>,
    log: crate::logging::Logger,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Access {
    pub fn new(state: &WebState) -> Self {
        let config = state.config();
        let mut settings = if config.explicit_data_dir {
            Settings::default()
        } else {
            load(&config.env_dir)
        };
        if let Some(key) = config.api_key.as_ref().filter(|k| !k.is_empty()) {
            settings.api_key = key.clone();
        }
        if let Some(password) = config.admin_password.as_ref().filter(|p| !p.is_empty()) {
            if settings.admin_salt.is_empty() {
                settings.admin_salt = secret();
            }
            if settings.admin_hash.is_empty() {
                settings.admin_hash = password_hash(password, &settings.admin_salt);
            }
        }
        let requested_lan = crate::config::host_is_lan(&config.host);
        Self {
            settings: Arc::new(std::sync::RwLock::new(settings)),
            env_dir: config.env_dir,
            listen: format!("{}:{}", config.host, config.port),
            requested_lan,
            logins: Arc::new(Mutex::new(LoginAttempts::default())),
            log: state.log.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn session_valid(&self, headers: &HeaderMap) -> bool {
        let Some(token) = cookie(headers, "codearts_admin") else {
            return false;
        };
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, expires| *expires > Instant::now());
        sessions.contains_key(token)
    }

    fn password_valid(&self, password: &str) -> bool {
        let settings = self.settings();
        if settings.admin_salt.is_empty() || settings.admin_hash.is_empty() {
            return false;
        }
        let hash = password_hash(password, &settings.admin_salt);
        equal_secret(hash.as_bytes(), settings.admin_hash.as_bytes())
    }

    async fn login_locked(&self) -> bool {
        self.logins
            .lock()
            .await
            .locked_until
            .is_some_and(|until| until > Instant::now())
    }

    async fn note_login(&self, success: bool) {
        let mut attempts = self.logins.lock().await;
        if success {
            *attempts = LoginAttempts::default();
            return;
        }
        attempts.failures += 1;
        if attempts.failures >= 5 {
            attempts.locked_until = Some(Instant::now() + Duration::from_secs(60));
            attempts.failures = 0;
        }
    }

    async fn create_session(&self) -> String {
        let token = secret();
        self.sessions.lock().await.insert(
            token.clone(),
            Instant::now() + Duration::from_secs(12 * 3600),
        );
        token
    }

    async fn logout(&self, headers: &HeaderMap) {
        if let Some(token) = cookie(headers, "codearts_admin") {
            self.sessions.lock().await.remove(token);
        }
    }

    fn save(&self, settings: Settings) -> Result<(), String> {
        save(&self.env_dir, &settings)?;
        *self.settings.write().unwrap_or_else(|e| e.into_inner()) = settings;
        Ok(())
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    header(headers, header::COOKIE.as_str())?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

fn proxy_key_valid(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return true;
    }
    let bearer = header(headers, header::AUTHORIZATION.as_str())
        .and_then(|value| value.strip_prefix("Bearer "));
    bearer
        .into_iter()
        .chain(header(headers, "x-api-key"))
        .any(|given| equal_secret(given.as_bytes(), expected.as_bytes()))
}

fn secure_response(mut response: Response, path: &str) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    if path.starts_with("/api/") {
        headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|kind| kind.starts_with("text/html"))
    {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; font-src 'self' https://cdn.jsdelivr.net; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap(),
        );
    }
    response
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = header(headers, header::ORIGIN.as_str()) else {
        return true;
    };
    let Some(host) = header(headers, header::HOST.as_str()) else {
        return false;
    };
    origin
        .parse::<axum::http::Uri>()
        .ok()
        .and_then(|uri| uri.authority().map(|a| a.as_str() == host))
        .unwrap_or(false)
}

const LOGIN_PAGE: &str = include_str!("../static/login.html");

async fn guard(
    State(access): State<Access>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let settings = access.settings();
    let path = request.uri().path().to_string();
    let headers = request.headers();
    if !settings.protected() {
        let response = next.run(request).await;
        return secure_response(response, &path);
    }
    if request.method() != Method::OPTIONS
        && request.method() != Method::GET
        && request.method() != Method::HEAD
        && !origin_allowed(headers)
    {
        return (StatusCode::FORBIDDEN, "跨站管理请求已拒绝").into_response();
    }
    if path.starts_with("/v1/") {
        if request.method() == Method::OPTIONS || proxy_key_valid(headers, &settings.api_key) {
            return next.run(request).await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"Invalid API key","type":"authentication_error"}})),
        )
            .into_response();
    }
    if path == "/api/access/login" {
        if access.login_locked().await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":"尝试次数过多，请一分钟后再试"})),
            )
                .into_response();
        }
        let response = next.run(request).await;
        access.note_login(response.status().is_success()).await;
        return secure_response(response, &path);
    }
    if path == "/api/access/session" {
        let response = next.run(request).await;
        return secure_response(response, &path);
    }
    if path == "/login.html"
        || path == "/login.js"
        || path == "/login.css"
        || path.starts_with("/vendor/")
    {
        let response = next.run(request).await;
        return secure_response(response, &path);
    }
    if path == "/oauth/callback" {
        let local = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| peer.ip().is_loopback());
        if local && origin_allowed(headers) {
            return next.run(request).await;
        }
        return StatusCode::NOT_FOUND.into_response();
    }
    if access.session_valid(headers).await {
        let response = next.run(request).await;
        return secure_response(response, &path);
    }
    if path.starts_with("/api/") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"管理登录已失效，请重新登录"})),
        )
            .into_response();
    }
    secure_response(Html(LOGIN_PAGE).into_response(), &path)
}

pub fn secure(router: Router, state: &WebState) -> Router {
    let access = Access::new(state);
    let public = Router::new()
        .route("/api/access/session", get(session))
        .route("/api/access/login", post(login))
        .with_state(access.clone());
    let protected = Router::new()
        .route("/api/access/logout", post(logout))
        .route(
            "/api/access/settings",
            get(access_settings).post(settings_update),
        )
        .with_state(access.clone());
    Router::new()
        .merge(public)
        .merge(protected)
        .merge(router)
        .layer(middleware::from_fn_with_state(access, guard))
}

async fn session(State(access): State<Access>, headers: HeaderMap) -> Response {
    let settings = access.settings();
    Json(json!({
        "protected": settings.protected(),
        "authenticated": !settings.protected() || access.session_valid(&headers).await,
        "lan": settings.lan.unwrap_or(false),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Login {
    password: String,
}

async fn login(State(access): State<Access>, Json(input): Json<Login>) -> Response {
    if !access.password_valid(&input.password) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"管理密码错误"})),
        )
            .into_response();
    }
    // Create a fresh token only after password validation.
    access.log.event("admin_login", json!({"ok": true}));
    let token = access.create_session().await;
    (
        [(
            header::SET_COOKIE,
            format!("codearts_admin={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=43200"),
        )],
        Json(json!({"ok":true})),
    )
        .into_response()
}

async fn logout(State(access): State<Access>, headers: HeaderMap) -> Response {
    access.logout(&headers).await;
    (
        [(
            header::SET_COOKIE,
            "codearts_admin=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response()
}

async fn access_settings(State(access): State<Access>) -> Response {
    let settings = access.settings();
    Json(json!({
        "lan": settings.lan.unwrap_or(false),
        "api_key": settings.api_key,
        "has_admin_password": !settings.admin_hash.is_empty(),
        "listen": access.listen.clone(),
        "restart_required": settings.lan.unwrap_or(false) != access.requested_lan,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Update {
    lan: bool,
    password: Option<String>,
    current_password: Option<String>,
}

async fn settings_update(State(access): State<Access>, Json(input): Json<Update>) -> Response {
    let old = access.settings();
    if !old.lan.unwrap_or(false) && input.lan && input.password.as_deref().unwrap_or("").len() < 12
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"首次开启请设置至少 12 位的管理密码"})),
        )
            .into_response();
    }
    if old.lan.unwrap_or(false)
        && !input.lan
        && !crate::config::host_is_loopback(&access.listen)
        && !access.password_valid(input.current_password.as_deref().unwrap_or(""))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"关闭局域网访问需要验证当前管理密码"})),
        )
            .into_response();
    }
    let mut settings = old.clone();
    settings.lan = Some(input.lan);
    if input.lan && settings.api_key.is_empty() {
        settings.api_key = format!("codearts-{}", secret());
    }
    if let Some(password) = input.password.as_deref().filter(|p| !p.is_empty()) {
        if password.len() < 12 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"管理密码至少 12 位"})),
            )
                .into_response();
        }
        settings.admin_salt = secret();
        settings.admin_hash = password_hash(password, &settings.admin_salt);
    }
    if let Err(e) = access.save(settings.clone()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":e}))).into_response();
    }
    access
        .log
        .event("access_settings", json!({"lan": settings.lan}));
    Json(json!({"ok":true,"lan":settings.lan.unwrap_or(false),"api_key":settings.api_key,"restart_required": settings.lan.unwrap_or(false) != access.requested_lan}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api, config::Config, models::Pool, usage::UsageStore};
    use axum::body::Body;
    use tower::ServiceExt;

    fn app(lan: bool) -> (Router, PathBuf) {
        let dir = std::env::temp_dir().join(format!("codearts-access-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = Settings {
            lan: Some(lan),
            api_key: "test-api-key".to_string(),
            admin_salt: "test-salt".to_string(),
            admin_hash: password_hash("correct-password", "test-salt"),
        };
        save(&dir, &settings).unwrap();
        let mut config = Config::load_desktop_dir(&dir).unwrap();
        config.host = if lan { "0.0.0.0" } else { "127.0.0.1" }.to_string();
        let state = WebState::new(
            config,
            Pool::new(Vec::new()),
            reqwest::Client::new(),
            Arc::new(UsageStore::new(None)),
        );
        (secure(api::router(state.clone()), &state), dir)
    }

    fn request(path: &str) -> Request<Body> {
        Request::builder().uri(path).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn lan_requires_admin_login_and_proxy_key() {
        let (app, dir) = app(true);
        assert_eq!(
            app.clone()
                .oneshot(request("/api/status"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request("/v1/models"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let login = Request::builder()
            .method(Method::POST)
            .uri("/api/access/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"password":"correct-password"}"#))
            .unwrap();
        let response = app.clone().oneshot(login).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let admin = Request::builder()
            .uri("/api/status")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(admin).await.unwrap().status(),
            StatusCode::OK
        );
        let proxy = Request::builder()
            .uri("/v1/models")
            .header(header::AUTHORIZATION, "Bearer test-api-key")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(proxy).await.unwrap().status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn login_page_carries_security_headers() {
        let (app, dir) = app(true);
        let response = app.oneshot(request("/")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::X_FRAME_OPTIONS).unwrap(),
            "DENY"
        );
        assert!(response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn wrong_password_never_creates_a_session() {
        let (app, dir) = app(true);
        let login = Request::builder()
            .method(Method::POST)
            .uri("/api/access/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"password":"wrong-password"}"#))
            .unwrap();
        let response = app.oneshot(login).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cross_site_management_write_is_rejected() {
        let (app, dir) = app(true);
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/access/login")
            .header(header::ORIGIN, "https://attacker.invalid")
            .header(header::HOST, "127.0.0.1:8787")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stored_password_is_not_plaintext() {
        let hash = password_hash("a-long-password", "fixed-salt");
        assert!(hash.starts_with("pbkdf2-sha256:"));
        assert!(!hash.contains("a-long-password"));
        assert_ne!(hash, password_hash("a-long-password", "another-salt"));
    }
}
