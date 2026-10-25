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
    /// true 表示该账号用 OAuth 凭证（auths/ 目录），ak/sk 留空
    #[serde(default)]
    oauth: bool,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub fn router() -> axum::Router<WebState> {
    axum::Router::new()
        .route("/api/status", get(status))
        .route("/api/accounts", get(list_accounts).post(add_account))
        .route(
            "/api/accounts/{id}",
            patch(update_account).delete(delete_account),
        )
        .route("/api/refresh", post(refresh_models))
        .route("/api/test", post(test_model))
        .route("/api/oauth/start", post(crate::oauth::flow::start))
        .route("/api/oauth/poll", post(crate::oauth::flow::poll))
        .route(
            "/api/oauth/account/{account}",
            axum::routing::delete(oauth_logout),
        )
        .route("/api/checkin/status", get(checkin_status))
        .route("/api/checkin/run", post(run_checkin))
        .route("/api/checkin/last", get(last_checkin))
        .route("/api/quota", get(quota))
        .route("/api/quota/refresh", post(quota_refresh))
        .route("/api/usage", get(usage_summary))
        .route("/api/usage/recent", get(usage_recent))
}

/// 上游每账号并发上限（与 api.rs 保持一致）。
fn concurrency_limit() -> usize {
    std::env::var("CODEARTS_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(3)
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

/// 解析账号的最终凭证：OAuth 账号从 auths/ 读取 STS 临时 AK/SK，
/// 必要时先续期；手动账号直接用 JSON 里的 AK/SK。
pub async fn resolve_account(
    http: &reqwest::Client,
    env_dir: &std::path::Path,
    account: &Account,
) -> Result<Account, String> {
    if !account.is_oauth() {
        return Ok(account.clone());
    }

    let Some(mut identity) = crate::oauth::store::load(env_dir, &account.id) else {
        return Err(format!("账号 {} 的 OAuth 凭证不存在，请重新登录", account.id));
    };

    // 快过期（30 分钟内）或已过期时先续期
    if identity.expired() || identity.expiring_within(1800) {
        match crate::oauth::store::refresh_and_save(http, env_dir, &identity).await {
            Ok(fresh) => {
                println!("[oauth] 账号 {} 凭证已续期", account.id);
                identity = fresh;
            }
            Err(e) => {
                if identity.expired() {
                    return Err(format!("账号 {} 凭证已过期且续期失败: {e}", account.id));
                }
                eprintln!("[oauth] 账号 {} 续期失败（继续用旧凭证）: {e}", account.id);
            }
        }
    }

    Ok(Account {
        id: account.id.clone(),
        ak: identity.access_key_id.clone(),
        sk: identity.secret_access_key.clone(),
        security_token: Some(identity.security_token.clone()),
        kind: account.kind.clone(),
        enabled: account.enabled,
    })
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
        // OAuth 账号需要先解析出 STS 凭证
        let resolved = match resolve_account(&state.http, &cfg.env_dir, account).await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[pool] 跳过账号 {}: {e}", account.id);
                continue;
            }
        };
        let cache_path = cfg.cache_path_for(account);
        let catalog = Catalog::new(
            resolved,
            &cache_path,
            cfg.benefit_fallback,
            state.http.clone(),
        );
        if !catalog.refresh().await {
            catalog.load_cache().await;
        }
        catalogs.push(Arc::new(catalog));
    }
    if catalogs.is_empty() {
        return Err("没有任何账号能成功初始化（OAuth 账号可能需要重新登录）".to_string());
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

    // OAuth 账号的登录状态（凭证有效期）
    let oauth_accounts: Vec<serde_json::Value> =
        crate::oauth::store::load_all(&cfg.env_dir)
            .iter()
            .map(|id| {
                json!({
                    "user_id": id.user_id,
                    "user_name": id.display_name(),
                    "remaining_secs": id.remaining_secs(),
                    "expires_at": id.expiration,
                    "expiring_soon": id.expiring_within(1800),
                })
            })
            .collect();

    // 每账号的并发/健康快照
    let mut health = serde_json::Map::new();
    for account in &cfg.accounts {
        let gate = state.gate(&account.id, concurrency_limit()).await;
        health.insert(account.id.clone(), gate.snapshot().await);
    }
    let pinned_sessions = state.pins.len().await;

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
            "oauth_count": oauth_accounts.len(),
            "pinned_sessions": pinned_sessions,
            "concurrency_limit": concurrency_limit(),
            "total_account_models": described.iter().map(|(_, _, n)| *n).sum::<usize>(),
            "listen": format!("{}:{}", cfg.host, cfg.port),
        },
        "oauth": oauth_accounts,
        "health": health,
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
        oauth: false,
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
        // OAuth 账号的 AK/SK 在 auths/ 里，需要先解析出来
        let resolved = match resolve_account(&state.http, &cfg.env_dir, account).await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[quota] 账号 {} 凭证不可用: {e}", account.id);
                state.quota.write().await.insert(
                    account.id.clone(),
                    crate::quota::Quota {
                        ok: false,
                        error: Some(e),
                        fetched_at: chrono::Utc::now().timestamp(),
                        ..Default::default()
                    },
                );
                continue;
            }
        };
        let quota = crate::quota::fetch_quota(
            &state.http,
            &resolved.ak,
            &resolved.sk,
            resolved.security_token(),
        )
        .await;
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

// ── 每日签到（AK/SK） ──────────────────────────────────────

#[derive(Serialize)]
struct AccountCheckin {
    account: String,
    ok: bool,
    message: String,
    claimed_today: bool,
    benefit_amount: f64,
    activities: Vec<crate::checkin::Activity>,
}

/// GET /api/checkin/status —— 各账号签到状态。
pub async fn checkin_status(State(state): State<WebState>) -> Response {
    let cfg = state.config();
    let mut accounts = Vec::new();
    let mut any_claimed = false;
    let mut all_ok = true;

    for account in cfg.accounts.iter().filter(|a| a.enabled) {
        let resolved = match resolve_account(&state.http, &cfg.env_dir, account).await {
            Ok(a) => a,
            Err(e) => {
                all_ok = false;
                accounts.push(AccountCheckin {
                    account: account.id.clone(),
                    ok: false,
                    message: e,
                    claimed_today: false,
                    benefit_amount: 0.0,
                    activities: Vec::new(),
                });
                continue;
            }
        };
        match crate::checkin::fetch_activities(
            &state.http,
            &resolved.ak,
            &resolved.sk,
            resolved.security_token(),
        )
        .await
        {
            Ok(activities) => {
                let daily = activities.iter().find(|a| a.is_daily());
                let claimed = daily.map(|a| a.claimed()).unwrap_or(false);
                if claimed {
                    any_claimed = true;
                }
                accounts.push(AccountCheckin {
                    account: account.id.clone(),
                    ok: true,
                    message: if claimed { "今日已签到" } else { "今日未签到" }.to_string(),
                    claimed_today: claimed,
                    benefit_amount: daily.map(|a| a.benefit_amount).unwrap_or(0.0),
                    activities,
                });
            }
            Err(e) => {
                all_ok = false;
                accounts.push(AccountCheckin {
                    account: account.id.clone(),
                    ok: false,
                    message: e,
                    claimed_today: false,
                    benefit_amount: 0.0,
                    activities: Vec::new(),
                });
            }
        }
    }

    Json(json!({
        "ok": all_ok,
        "claimed_today": any_claimed,
        "accounts": accounts,
        "account_count": cfg.accounts.iter().filter(|a| a.enabled).count(),
    }))
    .into_response()
}

/// POST /api/checkin/run —— 对所有启用账号执行签到。
pub async fn run_checkin(State(state): State<WebState>) -> Response {
    let cfg = state.config();
    let mut accounts = Vec::new();
    let mut all_ok = true;
    let mut any_claimed = false;

    for account in cfg.accounts.iter().filter(|a| a.enabled) {
        let resolved = match resolve_account(&state.http, &cfg.env_dir, account).await {
            Ok(a) => a,
            Err(e) => {
                all_ok = false;
                accounts.push(AccountCheckin {
                    account: account.id.clone(),
                    ok: false,
                    message: e,
                    claimed_today: false,
                    benefit_amount: 0.0,
                    activities: Vec::new(),
                });
                continue;
            }
        };
        let outcome = crate::checkin::run(
            &state.http,
            &resolved.ak,
            &resolved.sk,
            resolved.security_token(),
        )
        .await;
        println!(
            "[checkin] account={} ok={} claimed={} {}",
            account.id, outcome.ok, outcome.claimed_today, outcome.message
        );
        if outcome.ok {
            any_claimed = any_claimed || outcome.claimed_today;
        } else {
            all_ok = false;
        }
        accounts.push(AccountCheckin {
            account: account.id.clone(),
            ok: outcome.ok,
            message: outcome.message.clone(),
            claimed_today: outcome.claimed_today,
            benefit_amount: outcome.benefit_amount,
            activities: outcome.activities.clone(),
        });
        *state.last_checkin.write().await = Some(outcome);
    }

    Json(json!({
        "ok": all_ok,
        "claimed_today": any_claimed,
        "accounts": accounts,
    }))
    .into_response()
}

/// GET /api/checkin/last —— 最近一次结果。
pub async fn last_checkin(State(state): State<WebState>) -> Response {
    let last = state.last_checkin.read().await.clone();
    Json(json!({ "last": last })).into_response()
}

/// 后台每日签到：启动后 15 秒检查一次，之后每 30 分钟一轮。
pub fn spawn_checkin_scheduler(state: WebState) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        loop {
            let cfg = state.config();
            for account in cfg.accounts.iter().filter(|a| a.enabled) {
                let resolved = match resolve_account(&state.http, &cfg.env_dir, account).await {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("[checkin] account={} 凭证不可用: {e}", account.id);
                        continue;
                    }
                };
                match crate::checkin::fetch_activities(
                    &state.http,
                    &resolved.ak,
                    &resolved.sk,
                    resolved.security_token(),
                )
                .await
                {
                    Ok(activities) => {
                        let daily: Vec<_> = activities.iter().filter(|a| a.is_daily()).collect();
                        if daily.is_empty() || daily.iter().all(|a| a.claimed()) {
                            continue;
                        }
                        let outcome = crate::checkin::run(
                            &state.http,
                            &resolved.ak,
                            &resolved.sk,
                            resolved.security_token(),
                        )
                        .await;
                        println!(
                            "[checkin] 自动签到 account={} ok={} {}",
                            account.id, outcome.ok, outcome.message
                        );
                        *state.last_checkin.write().await = Some(outcome);
                    }
                    Err(e) => {
                        eprintln!("[checkin] account={} 状态检查失败: {e}", account.id);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        }
    });
}

// ── OAuth 账号 ──────────────────────────────────────────────

/// 保存 OAuth 登录得到的凭证，并把该账号登记到 accounts.json。
pub async fn save_oauth_identity(
    state: &WebState,
    identity: crate::oauth::Identity,
) -> Result<(), String> {
    let cfg = state.config();

    // 1) 凭证落盘到 auths/
    crate::oauth::store::save(&cfg.env_dir, &identity)?;

    // 2) 在 accounts.json 里标记为 oauth 账号（不写 AK/SK）
    let path = accounts_file_path(&cfg);
    let mut accounts = read_accounts(&path)?;
    let id = identity.user_id.clone();
    if !accounts.iter().any(|a| a.id == id) {
        accounts.push(AccountRecord {
            id: id.clone(),
            ak: String::new(),
            sk: String::new(),
            oauth: true,
            enabled: true,
        });
        write_accounts(&path, &accounts)?;
    }

    // 3) 重载配置并重建账号池
    state.reload_config()?;
    let _ = rebuild_pool(state).await;

    println!(
        "[oauth] 账号已保存: {} ({})",
        identity.display_name(),
        id
    );
    Ok(())
}

/// POST /api/oauth/logout —— 移除某个 OAuth 账号。
pub async fn oauth_logout(
    State(state): State<WebState>,
    Path(account): Path<String>,
) -> Response {
    let cfg = state.config();

    if let Err(e) = crate::oauth::store::remove(&cfg.env_dir, &account) {
        return err(StatusCode::NOT_FOUND, e);
    }

    // 同步从 accounts.json 移除
    let path = accounts_file_path(&cfg);
    if let Ok(mut accounts) = read_accounts(&path) {
        accounts.retain(|a| a.id != account);
        let _ = write_accounts(&path, &accounts);
    }

    let _ = state.reload_config();
    let _ = rebuild_pool(&state).await;

    Json(json!({ "ok": true })).into_response()
}
