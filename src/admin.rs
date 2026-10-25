//! 管理 API：供 WebUI 读取状态、增删改账号、测试模型。
//!
//! 账号变更会写回 accounts.json 并重建账号池，无需重启进程。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::{get, patch, post};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Account, Config};
use crate::models::{Catalog, Pool};
use crate::web::WebState;

pub fn router() -> axum::Router<WebState> {
    axum::Router::new()
        .route("/api/status", get(status))
        .route("/api/accounts", get(list_accounts).post(add_account))
        .route(
            "/api/accounts/{id}",
            patch(update_account).delete(delete_account),
        )
        .route("/api/refresh", post(refresh_models))
        .route("/api/quota", get(quota))
        .route("/api/quota/refresh", post(quota_refresh))
        .route("/api/usage", get(usage_summary))
        .route("/api/usage/recent", get(usage_recent))
        .route("/api/checkin/status", get(checkin_status))
        .route(
            "/api/checkin/credentials",
            post(save_checkin_credentials),
        )
        .route("/api/checkin/run", post(run_checkin))
        .route("/api/checkin/last", get(last_checkin))
        .route("/api/checkin/report", post(report_checkin))
        .route("/api/test", post(test_model))
}

#[derive(Serialize, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<AccountRecord>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AccountRecord {
    #[serde(default)]
    id: String,
    #[serde(default)]
    ak: String,
    #[serde(default)]
    sk: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 账号文件位置：优先 CODEARTS_ACCOUNTS，其次 .env 同目录 accounts.json。
pub fn accounts_file_path(cfg: &Config) -> PathBuf {
    std::env::var("CODEARTS_ACCOUNTS")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.env_dir.join("accounts.json"))
}

fn read_accounts(path: &PathBuf) -> Result<Vec<AccountRecord>, String> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败: {e}"))?;
    let parsed: AccountsFile =
        serde_json::from_str(&text).map_err(|e| format!("解析失败: {e}"))?;
    Ok(parsed.accounts)
}

fn write_accounts(path: &PathBuf, accounts: &[AccountRecord]) -> Result<(), String> {
    let payload = json!({ "accounts": accounts });
    let text = serde_json::to_string_pretty(&payload).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("写入失败: {e}"))
}

/// 校验账号列表是否会导致服务不可用。
fn validate_accounts(accounts: &[AccountRecord]) -> Result<(), String> {
    if accounts.is_empty() {
        return Err("至少要保留一个账号".to_string());
    }
    if !accounts.iter().any(|a| a.enabled) {
        return Err("至少要启用一个账号，否则代理无法提供任何模型".to_string());
    }
    Ok(())
}

/// 安全写回：先校验，再写盘，然后重建账号池；
/// 任一步失败都回滚文件，避免把配置写成下次启动会失败的状态。
async fn commit_accounts(
    state: &WebState,
    path: &PathBuf,
    old: &[AccountRecord],
    new: &[AccountRecord],
) -> Result<usize, Response> {
    if let Err(e) = validate_accounts(new) {
        return Err(err(StatusCode::BAD_REQUEST, e));
    }
    if let Err(e) = write_accounts(path, new) {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    if let Err(e) = state.reload_config() {
        // 配置解析失败：回滚文件，恢复原有池
        let _ = write_accounts(path, old);
        let _ = state.reload_config();
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("重载配置失败: {e}"),
        ));
    }
    match rebuild_pool(state).await {
        Ok(n) => Ok(n),
        Err(e) => {
            // 新配置无法构建出可用账号池：回滚
            let _ = write_accounts(path, old);
            let _ = state.reload_config();
            let _ = rebuild_pool(state).await;
            Err(err(StatusCode::BAD_REQUEST, e))
        }
    }
}

fn mask(ak: &str) -> String {
    if ak.len() <= 8 {
        return "****".to_string();
    }
    format!("{}****{}", &ak[..4], &ak[ak.len() - 2..])
}

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

/// 重建账号池（账号变更后调用）。
pub async fn rebuild_pool(state: &WebState) -> Result<usize, String> {
    let cfg = state.config();
    let enabled: Vec<&Account> = cfg.accounts.iter().filter(|a| a.enabled).collect();
    if enabled.is_empty() {
        return Err("没有启用的账号".to_string());
    }
    let mut catalogs = Vec::new();
    for account in enabled {
        let cache_path = cfg.cache_path_for(account);
        let catalog = Catalog::new(
            account.clone(),
            &cache_path,
            cfg.benefit_fallback,
            state.http.clone(),
        );
        if !catalog.refresh().await {
            catalog.load_cache().await;
        }
        catalogs.push(Arc::new(catalog));
    }
    let count = catalogs.len();
    *state.pool.write().await = Pool::new(catalogs);
    Ok(count)
}

/// GET /api/status —— 概览：账号、模型、运行状态。
pub async fn status(State(state): State<WebState>) -> Response {
    let pool = state.pool.read().await.clone();
    let cfg = state.config();
    let described = pool.describe().await;

    let mut accounts = Vec::new();
    for account in &cfg.accounts {
        let catalog = pool
            .catalogs()
            .iter()
            .find(|c| c.account_id() == account.id);
        let mut models = Vec::new();
        if let Some(c) = catalog {
            models = c.models().await.iter().map(|m| m.id.clone()).collect();
        }
        accounts.push(json!({
            "id": account.id,
            "ak": mask(&account.ak),
            "enabled": account.enabled,
            "models": models,
            "model_count": models.len(),
        }));
    }

    let merged = pool.merged_models().await;
    let model_list: Vec<Value> = merged
        .iter()
        .map(|m| {
            m.to_openai().as_object().map(|o| {
                let mut v = o.clone();
                v.insert("channel".to_string(), json!(m.channel));
                Value::Object(v)
            })
            .unwrap_or(Value::Null)
        })
        .collect();

    let total_models = model_list.len();
    Json(json!({
        "accounts": accounts,
        "models": model_list,
        "summary": {
            "account_count": cfg.accounts.len(),
            "enabled_count": cfg.accounts.iter().filter(|a| a.enabled).count(),
            "model_count": total_models,
            "total_account_models": described.iter().map(|(_, _, n)| *n).sum::<usize>(),
            "listen": format!("{}:{}", cfg.host, cfg.port),
        },
        "config": {
            "host": cfg.host,
            "port": cfg.port,
            "max_tokens_cap": cfg.max_tokens_cap,
            "benefit_fallback": cfg.benefit_fallback,
            "accounts_file": accounts_file_path(&cfg).display().to_string(),
        }
    }))
    .into_response()
}

/// GET /api/accounts —— 账号列表（密钥打码）。
pub async fn list_accounts(State(state): State<WebState>) -> Response {
    let cfg = state.config();
    let pool = state.pool.read().await.clone();
    let described = pool.describe().await;

    let items: Vec<Value> = cfg
        .accounts
        .iter()
        .map(|a| {
            let count = described
                .iter()
                .find(|(id, _, _)| id == &a.id)
                .map(|(_, _, n)| *n)
                .unwrap_or(0);
            json!({
                "id": a.id,
                "ak": mask(&a.ak),
                "enabled": a.enabled,
                "model_count": count,
            })
        })
        .collect();
    Json(json!({ "accounts": items })).into_response()
}

#[derive(Deserialize)]
pub struct AddAccountRequest {
    pub id: Option<String>,
    pub ak: String,
    pub sk: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// POST /api/accounts —— 新增账号。
pub async fn add_account(
    State(state): State<WebState>,
    Json(req): Json<AddAccountRequest>,
) -> Response {
    let ak = req.ak.trim().to_string();
    let sk = req.sk.trim().to_string();
    if ak.is_empty() || sk.is_empty() {
        return err(StatusCode::BAD_REQUEST, "ak 和 sk 不能为空");
    }
    // 防呆：华为云 AK 通常是 20 位大写字母数字
    if ak.len() < 16 || sk.len() < 16 {
        return err(StatusCode::BAD_REQUEST, "ak / sk 长度异常，请确认没有复制漏");
    }

    let path = accounts_file_path(&state.config());
    let mut accounts = match read_accounts(&path) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };

    if accounts.iter().any(|a| a.ak == ak) {
        return err(StatusCode::CONFLICT, "该 AK 已存在");
    }

    let mut id = req.id.unwrap_or_default().trim().to_string();
    if id.is_empty() {
        id = format!("account{}", accounts.len() + 1);
    }
    while accounts.iter().any(|a| a.id == id) {
        id.push('_');
    }

    let old = accounts.clone();
    accounts.push(AccountRecord {
        id: id.clone(),
        ak,
        sk,
        enabled: req.enabled,
    });

    match commit_accounts(&state, &path, &old, &accounts).await {
        Ok(n) => Json(json!({"ok": true, "id": id, "active_accounts": n})).into_response(),
        Err(resp) => resp,
    }
}

#[derive(Deserialize)]
pub struct UpdateAccountRequest {
    pub ak: Option<String>,
    pub sk: Option<String>,
    pub enabled: Option<bool>,
}

/// PATCH /api/accounts/{id} —— 修改账号（启用状态 / 密钥轮换）。
pub async fn update_account(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAccountRequest>,
) -> Response {
    let path = accounts_file_path(&state.config());
    let mut accounts = match read_accounts(&path) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let old = accounts.clone();

    if !accounts.iter().any(|a| a.id == id) {
        return err(StatusCode::NOT_FOUND, format!("账号 {id} 不存在"));
    }

    if let Some(enabled) = req.enabled {
        if let Some(t) = accounts.iter_mut().find(|a| a.id == id) {
            t.enabled = enabled;
        }
    }
    if let Some(ak) = req.ak {
        let ak = ak.trim().to_string();
        if ak.is_empty() {
            return err(StatusCode::BAD_REQUEST, "ak 不能为空");
        }
        if let Some(t) = accounts.iter_mut().find(|a| a.id == id) {
            t.ak = ak;
        }
    }
    if let Some(sk) = req.sk {
        let sk = sk.trim().to_string();
        if sk.is_empty() {
            return err(StatusCode::BAD_REQUEST, "sk 不能为空");
        }
        if let Some(t) = accounts.iter_mut().find(|a| a.id == id) {
            t.sk = sk;
        }
    }

    match commit_accounts(&state, &path, &old, &accounts).await {
        Ok(n) => Json(json!({"ok": true, "active_accounts": n})).into_response(),
        Err(resp) => resp,
    }
}

/// DELETE /api/accounts/{id} —— 删除账号。
pub async fn delete_account(
    State(state): State<WebState>,
    Path(id): Path<String>,
) -> Response {
    let path = accounts_file_path(&state.config());
    let mut accounts = match read_accounts(&path) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let old = accounts.clone();
    let before = accounts.len();
    accounts.retain(|a| a.id != id);
    if accounts.len() == before {
        return err(StatusCode::NOT_FOUND, format!("账号 {id} 不存在"));
    }

    match commit_accounts(&state, &path, &old, &accounts).await {
        Ok(n) => Json(json!({"ok": true, "active_accounts": n})).into_response(),
        Err(resp) => resp,
    }
}

/// POST /api/refresh —— 重新同步所有账号的模型。
pub async fn refresh_models(State(state): State<WebState>) -> Response {
    let pool = state.pool.read().await.clone();
    let mut report = Vec::new();
    for catalog in pool.catalogs() {
        let ok = catalog.refresh().await;
        report.push(json!({
            "account": catalog.account_id(),
            "ok": ok,
            "model_count": catalog.models().await.len(),
        }));
    }
    Json(json!({"ok": true, "accounts": report})).into_response()
}

#[derive(Deserialize)]
pub struct TestRequest {
    pub model: String,
    pub prompt: Option<String>,
    pub max_tokens: Option<i64>,
}

/// POST /api/test —— 用指定模型发一次真实请求，验证端到端可用。
pub async fn test_model(
    State(state): State<WebState>,
    Json(req): Json<TestRequest>,
) -> Response {
    let pool = state.pool.read().await.clone();
    let model = req.model.trim().to_string();
    if model.is_empty() {
        return err(StatusCode::BAD_REQUEST, "model 不能为空");
    }
    if !pool.contains(&model).await {
        return err(
            StatusCode::NOT_FOUND,
            format!("模型 {model} 不在可用列表中"),
        );
    }

    let prompt = req
        .prompt
        .unwrap_or_else(|| "只回复两个字：成功".to_string());
    let max_tokens = req.max_tokens.unwrap_or(512).clamp(16, 8192);

    // 用与正式请求相同的路径打一次自己的代理，确保测的是真实链路。
    let url = format!(
        "http://127.0.0.1:{}/v1/chat/completions",
        state.config().port
    );
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "max_tokens": max_tokens,
    });

    let started = std::time::Instant::now();
    let resp = state
        .http
        .post(&url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(180))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            let elapsed = started.elapsed().as_millis();
            let value: Value = serde_json::from_str(&text).unwrap_or(json!({"raw": text}));
            let content = value["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let reasoning = value["choices"][0]["message"]["reasoning_content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            Json(json!({
                "ok": status == 200 && value.get("choices").is_some(),
                "status": status,
                "elapsed_ms": elapsed,
                "content": content,
                "reasoning": reasoning,
                "usage": value.get("usage").cloned().unwrap_or(Value::Null),
                "error": value.get("error").cloned().unwrap_or(Value::Null),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "ok": false,
            "error": format!("请求失败: {e}"),
        }))
        .into_response(),
    }
}

// ── 额度与用量 ──────────────────────────────────────────────

/// 后台定时刷新每个账号的额度。
pub fn spawn_quota_refresher(state: WebState) {
    tokio::spawn(async move {
        // 启动后先等一会儿，避免和模型同步抢占上游连接
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        loop {
            refresh_all_quota(&state).await;
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }
    });
}

async fn refresh_all_quota(state: &WebState) -> usize {
    let cfg = state.config();
    let mut success = 0;
    for account in cfg.accounts.iter().filter(|a| a.enabled) {
        let quota = crate::quota::fetch_quota(&state.http, &account.ak, &account.sk).await;
        if quota.ok {
            success += 1;
        }
        state
            .quota
            .write()
            .await
            .insert(account.id.clone(), quota);
    }
    success
}

/// GET /api/quota —— 各账号额度（credit / token / 分套餐）。
pub async fn quota(State(state): State<WebState>) -> Response {
    // 缓存为空说明还没刷过，立即拉一次
    let empty = state.quota.read().await.is_empty();
    if empty {
        refresh_all_quota(&state).await;
    }

    let cfg = state.config();
    let cache = state.quota.read().await;
    let mut accounts = Vec::new();
    for account in &cfg.accounts {
        match cache.get(&account.id) {
            Some(q) => {
                let mut v = serde_json::to_value(q).unwrap_or_else(|_| json!({}));
                v["id"] = json!(account.id);
                v["enabled"] = json!(account.enabled);
                accounts.push(v);
            }
            None => accounts.push(json!({
                "id": account.id,
                "enabled": account.enabled,
                "ok": false,
                "error": "尚未获取",
            })),
        }
    }
    drop(cache);

    Json(json!({
        "accounts": accounts,
        "links": crate::quota::console_links(),
    }))
    .into_response()
}

/// POST /api/quota/refresh —— 立即刷新额度。
pub async fn quota_refresh(State(state): State<WebState>) -> Response {
    let n = refresh_all_quota(&state).await;
    Json(json!({"ok": true, "refreshed": n})).into_response()
}

#[derive(Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    pub hours: Option<i64>,
    #[serde(default)]
    pub bucket: Option<String>,
}

/// GET /api/usage —— 用量汇总与趋势。
pub async fn usage_summary(
    State(state): State<WebState>,
    axum::extract::Query(q): axum::extract::Query<UsageQuery>,
) -> Response {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 90);
    let bucket = q.bucket.as_deref().unwrap_or("hour");
    let since = chrono::Utc::now().timestamp() - hours * 3600;
    let summary = state.usage.summarize(since, bucket).await;
    Json(summary).into_response()
}

/// GET /api/usage/recent —— 最近请求明细。
pub async fn usage_recent(
    State(state): State<WebState>,
    axum::extract::Query(q): axum::extract::Query<UsageQuery>,
) -> Response {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 90);
    let since = chrono::Utc::now().timestamp() - hours * 3600;
    let records = state.usage.recent(200, since).await;
    let activity = state.usage.activity_by_model().await;
    Json(json!({
        "records": records,
        "retained": state.usage.len().await,
        "activity": activity
            .into_iter()
            .map(|(model, (last_ts, count))| (model, json!({"last_ts": last_ts, "count": count})))
            .collect::<serde_json::Map<_, _>>(),
    }))
    .into_response()
}

// ── 每日签到 ────────────────────────────────────────────────

fn checkin_creds_path(cfg: &Config) -> PathBuf {
    cfg.env_dir.join("checkin.json")
}

fn read_checkin_creds(cfg: &Config) -> crate::checkin::CheckinCredentials {
    let path = checkin_creds_path(cfg);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_checkin_creds(
    cfg: &Config,
    creds: &crate::checkin::CheckinCredentials,
) -> Result<(), String> {
    let path = checkin_creds_path(cfg);
    let text = serde_json::to_string_pretty(creds).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

#[derive(Deserialize)]
pub struct SaveCheckinCreds {
    pub cookie: String,
    #[serde(default)]
    pub cftk: String,
}

/// POST /api/checkin/credentials —— 保存浏览器导出的 Cookie 与 cftk。
pub async fn save_checkin_credentials(
    State(state): State<WebState>,
    Json(req): Json<SaveCheckinCreds>,
) -> Response {
    let cfg = state.config();
    let cookie = req.cookie.trim().to_string();
    if cookie.is_empty() {
        return err(StatusCode::BAD_REQUEST, "Cookie 不能为空");
    }

    // cookie 里通常本身就含 cftk，允许用户只贴 cookie
    let mut cftk = req.cftk.trim().to_string();
    if cftk.is_empty() {
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("cftk=") {
                cftk = v.to_string();
                break;
            }
        }
    }

    let creds = crate::checkin::CheckinCredentials {
        cookie,
        cftk,
        updated_at: chrono::Utc::now().timestamp(),
    };

    if let Err(e) = write_checkin_creds(&cfg, &creds) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }

    // 保存后立即验证一次，让用户马上知道能不能用
    let result = crate::checkin::fetch_activities(&state.http, &creds).await;
    match result {
        Ok(activities) => Json(json!({
            "ok": true,
            "verified": true,
            "activity_count": activities.len(),
            "activities": activities,
        }))
        .into_response(),
        Err(e) => Json(json!({
            "ok": true,
            "verified": false,
            "warning": e,
        }))
        .into_response(),
    }
}

/// GET /api/checkin/status —— 签到状态与活动列表。
pub async fn checkin_status(State(state): State<WebState>) -> Response {
    let cfg = state.config();
    let creds = read_checkin_creds(&cfg);

    if !creds.is_configured() {
        return Json(json!({
            "configured": false,
            "activities": [],
            "message": "尚未配置 Cookie。请在已登录的浏览器里打开签到页，导出 Cookie 后粘贴到下方。",
            "signin_url": "https://devcloud.cn-north-4.huaweicloud.com/chat/home",
        }))
        .into_response();
    }

    match crate::checkin::fetch_activities(&state.http, &creds).await {
        Ok(activities) => Json(json!({
            "configured": true,
            "valid": true,
            "activities": activities,
            "claimed_today": crate::checkin::all_daily_claimed(&activities),
            "updated_at": creds.updated_at,
        }))
        .into_response(),
        Err(e) => Json(json!({
            "configured": true,
            "valid": false,
            "activities": [],
            "message": e,
            "updated_at": creds.updated_at,
        }))
        .into_response(),
    }
}

/// POST /api/checkin/run —— 立即签到一次。
pub async fn run_checkin(State(state): State<WebState>) -> Response {
    let cfg = state.config();
    let creds = read_checkin_creds(&cfg);
    if !creds.is_configured() {
        return err(StatusCode::BAD_REQUEST, "尚未配置 Cookie");
    }
    let result = crate::checkin::do_checkin(&state.http, &creds).await;
    *state.last_checkin.write().await = Some(result.clone());
    let status = if result.ok {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(json!(result))).into_response()
}

/// GET /api/checkin/last —— 最近一次签到结果。
pub async fn last_checkin(State(state): State<WebState>) -> Response {
    let last = state.last_checkin.read().await.clone();
    Json(json!({ "last": last })).into_response()
}

/// 后台每日签到：启动后检查一次，之后每 30 分钟检查（未签到才执行）。
pub fn spawn_checkin_scheduler(state: WebState) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        loop {
            let cfg = state.config();
            let creds = read_checkin_creds(&cfg);
            if creds.is_configured() {
                // 只在今天还没签到时执行，避免无谓请求
                match crate::checkin::fetch_activities(&state.http, &creds).await {
                    Ok(activities) => {
                        if !crate::checkin::all_daily_claimed(&activities)
                            && crate::checkin::find_daily_candidate(&activities).is_some()
                        {
                            let result = crate::checkin::do_checkin(&state.http, &creds).await;
                            println!(
                                "[checkin] 自动签到: ok={} {}",
                                result.ok, result.message
                            );
                            *state.last_checkin.write().await = Some(result);
                        }
                    }
                    Err(e) => {
                        eprintln!("[checkin] 状态检查失败: {e}");
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        }
    });
}

#[derive(Deserialize)]
pub struct ReportCheckin {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub campaign_id: Option<String>,
    #[serde(default)]
    pub benefit_amount: Option<f64>,
    /// 页面里读到的 cookie（可能不含 HttpOnly 部分，仅用于后续自动签到）
    #[serde(default)]
    pub cookie: Option<String>,
    #[serde(default)]
    pub cftk: Option<String>,
}

/// POST /api/checkin/report —— 浏览器内脚本执行完签到后回传结果。
///
/// 这条路径用于绕过 HttpOnly cookie：签到在已登录页面里发起（浏览器自动带
/// Cookie），本服务只负责记录结果，并在脚本顺带提供了 cookie 时保存下来供
/// 后续自动签到尝试。
pub async fn report_checkin(
    State(state): State<WebState>,
    body: axum::body::Bytes,
) -> Response {
    // 浏览器端用 text/plain 规避预检，这里手动解析，不依赖 Content-Type。
    let req: ReportCheckin = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("JSON 解析失败: {e}")),
    };
    let cfg = state.config();

    if let (Some(cookie), Some(cftk)) = (req.cookie.as_ref(), req.cftk.as_ref()) {
        let cookie = cookie.trim().to_string();
        if !cookie.is_empty() {
            let creds = crate::checkin::CheckinCredentials {
                cookie,
                cftk: cftk.trim().to_string(),
                updated_at: chrono::Utc::now().timestamp(),
            };
            let _ = write_checkin_creds(&cfg, &creds);
        }
    }

    let result = crate::checkin::CheckinResult {
        ok: req.ok,
        action: "browser".to_string(),
        message: if req.message.is_empty() {
            if req.ok { "签到成功".to_string() } else { "签到失败".to_string() }
        } else {
            req.message.clone()
        },
        campaign_id: req.campaign_id.clone(),
        benefit_amount: req.benefit_amount,
        claimed_today: req.ok,
        activities: Vec::new(),
        at: chrono::Utc::now().timestamp(),
    };

    println!(
        "[checkin] 浏览器回传: ok={} {}",
        result.ok, result.message
    );
    *state.last_checkin.write().await = Some(result.clone());

    Json(json!({ "ok": true, "recorded": result })).into_response()
}
