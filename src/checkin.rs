//! 每日签到（活动福利）。
//!
//! CodeArts 的签到走 `PromptCenterService/v1/ops/*`，鉴权是**浏览器 Cookie +
//! cftk(CSRF token)**，不是 AK/SK。因此这里不接受账号密钥，而是接收用户从
//! 已登录浏览器里导出的 Cookie 与 cftk。
//!
//! 调用序列（与官方前端一致）：
//!   1. GET  /ops/delivery?channel=WEB   —— 拉取活动列表，找到 daily_claim 条目
//!   2. POST /ops/claim                  —— 领取，body 需 campaignId + idempotentKey
//!   3. POST /ops/confirm                —— 确认到账
//!
//! 这些请求必须带 `credentials: same-origin` 的 Cookie，且 POST 需要 cftk 头，
//! 否则服务端返回 `{"error":"csrf validate fail."}`。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PC_BASE: &str = "https://devcloud.cn-north-4.huaweicloud.com";
const OPS_DELIVERY: &str = "/PromptCenterService/v1/ops/delivery?channel=WEB";
const OPS_CLAIM: &str = "/PromptCenterService/v1/ops/claim";
const OPS_CONFIRM: &str = "/PromptCenterService/v1/ops/confirm";

/// 用户从浏览器导出的登录凭证。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CheckinCredentials {
    /// 完整的 Cookie 头内容，例如 `cftk=xxx; session=yyy`
    pub cookie: String,
    /// CSRF token，对应 cookie 里的 cftk 值
    pub cftk: String,
    /// 记录最近一次更新时间
    #[serde(default)]
    pub updated_at: i64,
}

impl CheckinCredentials {
    pub fn is_configured(&self) -> bool {
        !self.cookie.trim().is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityItem {
    pub campaign_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub claimed: bool,
    pub benefit_amount: Option<f64>,
    pub claimable: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CheckinResult {
    pub ok: bool,
    pub action: String,
    pub message: String,
    pub campaign_id: Option<String>,
    pub benefit_amount: Option<f64>,
    pub claimed_today: bool,
    pub activities: Vec<ActivityItem>,
    pub at: i64,
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 从活动列表原始 JSON 中挑出签到类条目。
///
/// 官方前端把 DAILY_CLAIM / daily_claim / USER_LOGIN / login 都视作每日签到，
/// 这里保持同样的判定，避免因大小写差异漏掉。
fn normalize_kind(raw: &str) -> String {
    match raw {
        "DAILY_CLAIM" | "daily_claim" => "daily_claim".to_string(),
        "USER_LOGIN" | "user_login" | "login" => "login".to_string(),
        "INVITE_USER" | "invite_user" => "invite".to_string(),
        "STUDENT_CERTIFIED" | "student_certified" => "student_certify".to_string(),
        "NEW_USER_REGISTER" | "new_user_register" => "register".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

fn is_claimed_status(status: &str) -> bool {
    matches!(status, "CLAIMED" | "CONFIRM" | "CONSUME")
}

pub fn parse_activities(value: &Value) -> Vec<ActivityItem> {
    // 数据可能包在 data / result 里，逐层剥
    let mut node = value;
    for _ in 0..4 {
        if let Some(data) = node.get("data") {
            if data.is_array() || data.is_object() {
                node = data;
                continue;
            }
        }
        if let Some(result) = node.get("result") {
            if result.is_array() || result.is_object() {
                node = result;
                continue;
            }
        }
        break;
    }

    let list: Vec<&Value> = if let Some(arr) = node.as_array() {
        arr.iter().collect()
    } else if let Some(arr) = node.get("activities").and_then(|v| v.as_array()) {
        arr.iter().collect()
    } else if let Some(arr) = node.get("list").and_then(|v| v.as_array()) {
        arr.iter().collect()
    } else {
        Vec::new()
    };

    list.into_iter()
        .filter_map(|item| {
            let campaign_id = item
                .get("campaignId")
                .or_else(|| item.get("id"))
                .and_then(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })?;
            let raw_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let kind = normalize_kind(raw_type);
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let claimed = is_claimed_status(&status)
                || item.get("claimed").and_then(|v| v.as_bool()).unwrap_or(false);
            let benefit_amount = item
                .get("benefitAmount")
                .or_else(|| item.get("pendingTotalAmount"))
                .and_then(|v| v.as_f64());
            let claimable = item.get("claimable").and_then(|v| v.as_bool()).unwrap_or(true);
            Some(ActivityItem {
                campaign_id,
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                kind,
                status,
                claimed,
                benefit_amount,
                claimable,
            })
        })
        .collect()
}

/// 找出今天还能签到的条目。
pub fn find_daily_candidate(activities: &[ActivityItem]) -> Option<&ActivityItem> {
    activities.iter().find(|a| {
        (a.kind == "daily_claim" || a.kind == "login")
            && !a.claimed
            && a.claimable
            && a.status != "ELIGIBLE"
    })
}

/// 是否所有签到条目今天已完成。
pub fn all_daily_claimed(activities: &[ActivityItem]) -> bool {
    let daily: Vec<&ActivityItem> = activities
        .iter()
        .filter(|a| a.kind == "daily_claim" || a.kind == "login")
        .collect();
    !daily.is_empty() && daily.iter().all(|a| a.claimed || a.status == "ELIGIBLE")
}

struct Authed {
    cookie: String,
    cftk: String,
}

impl Authed {
    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![
            ("Agent-Type", "PromptCenter".to_string()),
            ("X-Language", "zh-cn".to_string()),
            ("Accept", "application/json, text/plain, */*".to_string()),
            ("Origin", PC_BASE.to_string()),
            ("Referer", format!("{PC_BASE}/chat/home")),
            ("Cookie", self.cookie.clone()),
        ];
        if !self.cftk.is_empty() {
            h.push(("cftk", self.cftk.clone()));
        }
        h
    }
}

/// 拉取活动列表（也用于检查登录态是否有效）。
pub async fn fetch_activities(
    http: &reqwest::Client,
    creds: &CheckinCredentials,
) -> Result<Vec<ActivityItem>, String> {
    if !creds.is_configured() {
        return Err("未配置 Cookie，请先在「签到」页粘贴浏览器 Cookie".to_string());
    }
    let authed = Authed {
        cookie: creds.cookie.clone(),
        cftk: creds.cftk.clone(),
    };
    let url = format!("{PC_BASE}{OPS_DELIVERY}");
    let mut req = http.get(&url);
    for (k, v) in authed.headers() {
        req = req.header(k, v);
    }

    let resp = req
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = resp.text().await.unwrap_or_default();

    if ct.contains("text/html") {
        return Err("Cookie 已失效：服务端返回了登录页，请重新从浏览器复制".to_string());
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", text.chars().take(200).collect::<String>()));
    }

    let value: Value = serde_json::from_str(&text)
        .map_err(|e| format!("响应解析失败: {e}; body={}", text.chars().take(200).collect::<String>()))?;

    if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
        return Err(format!("服务端错误: {err}"));
    }
    // code 非 0/200 视为失败
    if let Some(code) = value.get("code").and_then(|v| v.as_i64()) {
        if code != 0 && code != 200 {
            return Err(format!(
                "服务端返回 code={code}: {}",
                value.get("message").and_then(|v| v.as_str()).unwrap_or("")
            ));
        }
    }

    Ok(parse_activities(&value))
}

/// 执行一次签到：claim + confirm。
pub async fn do_checkin(
    http: &reqwest::Client,
    creds: &CheckinCredentials,
) -> CheckinResult {
    let mut result = CheckinResult {
        at: now_ts(),
        action: "checkin".to_string(),
        ..Default::default()
    };

    // 1) 拉活动列表
    let activities = match fetch_activities(http, creds).await {
        Ok(a) => a,
        Err(e) => {
            result.message = e;
            return result;
        }
    };
    result.activities = activities.clone();

    if all_daily_claimed(&activities) {
        result.ok = true;
        result.claimed_today = true;
        result.message = "今日已签到，无需重复领取".to_string();
        return result;
    }

    let Some(candidate) = find_daily_candidate(&activities).cloned() else {
        result.ok = true;
        result.claimed_today = true;
        result.message = "今日没有可领取的签到奖励".to_string();
        return result;
    };

    result.campaign_id = Some(candidate.campaign_id.clone());
    result.benefit_amount = candidate.benefit_amount;

    let authed = Authed {
        cookie: creds.cookie.clone(),
        cftk: creds.cftk.clone(),
    };

    // 2) claim
    let idempotent = format!("{}_{}", candidate.campaign_id, chrono::Utc::now().timestamp_millis());
    let claim_body = json!({
        "campaignId": candidate.campaign_id,
        "idempotentKey": idempotent,
        "channel": "WEB",
    })
    .to_string();

    let claim_url = format!("{PC_BASE}{OPS_CLAIM}");
    let mut req = http
        .post(&claim_url)
        .body(claim_body)
        .header("Content-Type", "application/json");
    for (k, v) in authed.headers() {
        req = req.header(k, v);
    }
    let claim_resp = match req.timeout(std::time::Duration::from_secs(30)).send().await {
        Ok(r) => r,
        Err(e) => {
            result.message = format!("领取请求失败: {e}");
            return result;
        }
    };

    let claim_status = claim_resp.status();
    let claim_ct = claim_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let claim_text = claim_resp.text().await.unwrap_or_default();

    if claim_ct.contains("text/html") {
        result.message = "Cookie 已失效：领取时被重定向到登录页".to_string();
        return result;
    }

    let claim_json: Value = serde_json::from_str(&claim_text).unwrap_or(Value::Null);
    if let Some(err) = claim_json.get("error").and_then(|v| v.as_str()) {
        result.message = if err.contains("csrf") {
            "CSRF 校验失败：cftk 与 Cookie 不匹配，请重新复制".to_string()
        } else {
            format!("领取失败: {err}")
        };
        return result;
    }
    if !claim_status.is_success() {
        result.message = format!(
            "领取失败 HTTP {claim_status}: {}",
            claim_text.chars().take(200).collect::<String>()
        );
        return result;
    }

    // 官方前端把 code 0/200 或存在 data 视为成功
    let claim_ok = claim_json
        .get("code")
        .and_then(|v| v.as_i64())
        .map(|c| c == 0 || c == 200)
        .unwrap_or_else(|| claim_json.get("data").map(|d| !d.is_null()).unwrap_or(false));

    if !claim_ok {
        result.message = format!(
            "领取未成功: {}",
            claim_json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or(&claim_text.chars().take(200).collect::<String>())
        );
        return result;
    }

    // 3) confirm
    let confirm_body = json!({ "campaignId": candidate.campaign_id }).to_string();
    let confirm_url = format!("{PC_BASE}{OPS_CONFIRM}");
    let mut creq = http
        .post(&confirm_url)
        .body(confirm_body)
        .header("Content-Type", "application/json");
    for (k, v) in authed.headers() {
        creq = creq.header(k, v);
    }
    let confirm_ok = match creq.timeout(std::time::Duration::from_secs(30)).send().await {
        Ok(r) => {
            let text = r.text().await.unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            v.get("code")
                .and_then(|c| c.as_i64())
                .map(|c| c == 0 || c == 200)
                .unwrap_or(true)
        }
        Err(_) => false,
    };

    result.ok = true;
    result.claimed_today = true;
    result.message = if confirm_ok {
        match candidate.benefit_amount {
            Some(amount) if amount > 0.0 => format!("签到成功，获得 {amount} 积分"),
            _ => "签到成功".to_string(),
        }
    } else {
        "领取成功，但确认步骤未完成（通常稍后会自动到账）".to_string()
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_daily_claim_activity() {
        let raw = json!({
            "code": 0,
            "data": [
                {
                    "campaignId": "abc123",
                    "type": "DAILY_CLAIM",
                    "title": "每日签到",
                    "status": "PENDING",
                    "benefitAmount": 10,
                    "claimable": true
                }
            ]
        });
        let acts = parse_activities(&raw);
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].kind, "daily_claim");
        assert_eq!(acts[0].campaign_id, "abc123");
        assert!(!acts[0].claimed);
        assert!(find_daily_candidate(&acts).is_some());
    }

    #[test]
    fn recognizes_claimed_status() {
        let raw = json!([
            {"campaignId": "1", "type": "daily_claim", "status": "CLAIMED", "claimable": true}
        ]);
        let acts = parse_activities(&raw);
        assert!(acts[0].claimed);
        assert!(find_daily_candidate(&acts).is_none());
        assert!(all_daily_claimed(&acts));
    }

    #[test]
    fn handles_lowercase_and_login_types() {
        let raw = json!([
            {"campaignId": 2, "type": "user_login", "status": "", "claimable": true}
        ]);
        let acts = parse_activities(&raw);
        assert_eq!(acts[0].kind, "login");
        assert_eq!(acts[0].campaign_id, "2");
    }

    #[test]
    fn missing_cookie_is_reported() {
        let creds = CheckinCredentials::default();
        assert!(!creds.is_configured());
    }
}
