//! 每日签到（活动福利）—— 纯 AK/SK 实现。
//!
//! 端点位于 snap-access 网关（与聊天同一域名），用华为云 SDK-HMAC-SHA256 签名：
//!   GET  /v1/ops/delivery?channel=IDE            活动列表
//!   POST /v1/ops/claim   {campaignId, channel, idempotentKey}
//!   POST /v1/ops/confirm {campaignId}
//!
//! 关键点（缺一不可，否则 401 APIG.0301 verify ak sk signature fail）：
//!   - 必须携带 `X-Sdk-Content-Sha256` 头，并用它作为 payload hash；
//!   - 参与签名的头集合必须与实际发送的完全一致；
//!   - 使用临时凭证时需额外签名 `X-Security-Token`。
//!
//! channel 要用 `IDE`：WEB 渠道走浏览器会话鉴权，IDE 渠道才认 AK/SK 签名。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::signer::Signer;

pub const SNAP_BASE: &str = "https://snap-access.cn-north-4.myhuaweicloud.com";
const EP_DELIVERY: &str = "/v1/ops/delivery";
const EP_CLAIM: &str = "/v1/ops/claim";
const EP_CONFIRM: &str = "/v1/ops/confirm";
const CHANNEL: &str = "IDE";

/// 每日签到活动的 campaignId（上游固定为 1）。
const DAILY_ID: i64 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Activity {
    #[serde(rename = "campaignId", alias = "campaign_id", default)]
    pub campaign_id: i64,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "benefitAmount", default)]
    pub benefit_amount: f64,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub claimable: bool,
    #[serde(rename = "benefitUnit", default)]
    pub benefit_unit: String,
}

impl Activity {
    pub fn claimed(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("CLAIMED") | Some("CONFIRMED") | Some("CONSUME") | Some("CONSUMED")
        )
    }

    pub fn is_daily(&self) -> bool {
        self.campaign_id == DAILY_ID
            || self.kind.eq_ignore_ascii_case("USER_LOGIN")
            || self.kind.eq_ignore_ascii_case("DAILY_CLAIM")
    }
}

/// 同一个账号可能同时有多个每日签到活动，不能只检查列表中的第一项。
pub fn daily_completed(activities: &[Activity]) -> bool {
    let mut found = false;
    activities.iter().filter(|a| a.is_daily()).all(|a| {
        found = true;
        a.claimed()
    }) && found
}

fn is_benign_claim_error(error: &str) -> bool {
    error.contains("尚未到达") || error.contains("已领取")
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
    pub claimed_today: bool,
    pub benefit_amount: f64,
    pub activities: Vec<Activity>,
    pub at: i64,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Ctx<'a> {
    http: &'a reqwest::Client,
    ak: &'a str,
    sk: &'a str,
    security_token: Option<&'a str>,
    base: &'a str,
}

impl<'a> Ctx<'a> {
    async fn call(
        &self,
        method: &str,
        path_query: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path_query);
        let payload = match body {
            Some(v) => serde_json::to_vec(&v).map_err(|e| format!("序列化失败: {e}"))?,
            None => Vec::new(),
        };

        let signer = Signer::new(self.ak, self.sk);
        let signed = signer.sign_full(
            method,
            &url,
            &payload,
            &[
                ("content-type", "application/json"),
                ("agent-type", "PromptCenter"),
                ("x-language", "zh-cn"),
                ("accept", "application/json"),
            ],
            self.security_token,
        );

        let mut req = self
            .http
            .request(method.parse().unwrap_or(reqwest::Method::GET), &url);
        for (k, v) in signed.headers {
            req = req.header(k, v);
        }
        if !payload.is_empty() {
            req = req.body(payload);
        }

        let resp = req
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| format!("请求失败: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                text.chars().take(220).collect::<String>()
            ));
        }

        let value: Value = serde_json::from_str(&text).map_err(|e| {
            format!(
                "解析失败: {e}; body={}",
                text.chars().take(200).collect::<String>()
            )
        })?;

        if let Some(code) = value.get("code").and_then(|v| v.as_i64()) {
            if code != 0 {
                return Err(format!(
                    "code={code}: {}",
                    value.get("message").and_then(|v| v.as_str()).unwrap_or("")
                ));
            }
        }
        Ok(value)
    }
}

/// 解析活动列表（兼容 data.items / data 数组）。
pub fn parse_activities(value: &Value) -> Vec<Activity> {
    let items = value
        .get("data")
        .and_then(|d| d.get("items"))
        .and_then(|v| v.as_array())
        .or_else(|| value.get("data").and_then(|d| d.as_array()))
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();

    items
        .into_iter()
        .filter_map(|it| serde_json::from_value::<Activity>(it).ok())
        .filter(|a| a.campaign_id != 0 || !a.title.is_empty())
        .collect()
}

/// 查询活动列表。
pub async fn fetch_activities(
    http: &reqwest::Client,
    ak: &str,
    sk: &str,
    security_token: Option<&str>,
) -> Result<Vec<Activity>, String> {
    fetch_activities_at(http, SNAP_BASE, ak, sk, security_token).await
}

async fn fetch_activities_at(
    http: &reqwest::Client,
    base: &str,
    ak: &str,
    sk: &str,
    security_token: Option<&str>,
) -> Result<Vec<Activity>, String> {
    let ctx = Ctx {
        http,
        ak,
        sk,
        security_token,
        base,
    };
    let value = ctx
        .call("GET", &format!("{EP_DELIVERY}?channel={CHANNEL}"), None)
        .await?;
    Ok(parse_activities(&value))
}

/// 执行签到：查询 → 逐个领取并确认 → 复查到账状态。
pub async fn run(
    http: &reqwest::Client,
    ak: &str,
    sk: &str,
    security_token: Option<&str>,
) -> Outcome {
    run_at(http, SNAP_BASE, ak, sk, security_token).await
}

async fn run_at(
    http: &reqwest::Client,
    base: &str,
    ak: &str,
    sk: &str,
    security_token: Option<&str>,
) -> Outcome {
    let mut out = Outcome {
        at: now(),
        ..Default::default()
    };
    let ctx = Ctx {
        http,
        ak,
        sk,
        security_token,
        base,
    };

    let activities = match fetch_activities_at(http, base, ak, sk, security_token).await {
        Ok(a) => a,
        Err(e) => {
            out.message = e;
            return out;
        }
    };
    out.activities = activities.clone();

    if daily_completed(&activities) {
        out.ok = true;
        out.claimed_today = true;
        out.message = "今日已签到".to_string();
        return out;
    }

    let daily: Vec<_> = activities.iter().filter(|a| a.is_daily()).collect();
    if daily.is_empty() {
        out.message = "未找到每日签到活动".to_string();
        return out;
    }

    let mut attempted = 0;
    let mut claimed_amount = 0.0;
    let mut errors = Vec::new();
    for activity in daily {
        if activity.claimed() || !activity.claimable {
            continue;
        }
        attempted += 1;
        let idem = format!(
            "claim_{}_{}",
            activity.campaign_id,
            chrono::Utc::now().timestamp_millis()
        );
        let claim = json!({
            "campaignId": activity.campaign_id,
            "channel": CHANNEL,
            "idempotentKey": idem,
        });
        if let Err(e) = ctx.call("POST", EP_CLAIM, Some(claim)).await {
            if !is_benign_claim_error(&e) {
                errors.push(format!("{}：{e}", activity.title));
                continue;
            }
            if e.contains("已领取") {
                let _ = ctx
                    .call(
                        "POST",
                        EP_CONFIRM,
                        Some(json!({ "campaignId": activity.campaign_id })),
                    )
                    .await;
            }
        } else {
            // 确认失败也要继续处理其他每日活动，最终由复查判断是否到账。
            let _ = ctx
                .call(
                    "POST",
                    EP_CONFIRM,
                    Some(json!({ "campaignId": activity.campaign_id })),
                )
                .await;
            claimed_amount += activity.benefit_amount;
        }
    }

    if attempted == 0 && errors.is_empty() {
        out.message = "今日签到活动暂不可领取，将自动重试".to_string();
        return out;
    }

    match fetch_activities_at(http, base, ak, sk, security_token).await {
        Ok(refreshed) => {
            out.claimed_today = daily_completed(&refreshed);
            out.ok = out.claimed_today && errors.is_empty();
            out.activities = refreshed;
        }
        Err(e) => errors.push(format!("签到后复查失败：{e}")),
    }
    out.benefit_amount = claimed_amount;

    if out.claimed_today && errors.is_empty() {
        out.message = if claimed_amount > 0.0 {
            format!("签到成功，获得 {:.0} 积分", claimed_amount)
        } else {
            "今日已签到".to_string()
        };
    } else if out.claimed_today {
        out.message = format!("每日活动已到账，但有 {} 项领取失败", errors.len());
    } else if !errors.is_empty() {
        out.message = format!("签到未完成：{}", errors.join("；"));
    } else {
        out.message = "签到请求已提交，仍有每日活动未到账，将自动重试".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct FakeOps {
        activities: Vec<Value>,
        calls: Vec<String>,
        fail_claim: Option<i64>,
        fail_confirm: Option<i64>,
        claim_marks_claimed: bool,
    }

    async fn fake_server(state: Arc<Mutex<FakeOps>>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let state = state.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    let (method, path, body) = loop {
                        let n = socket.read(&mut buffer).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..n]);
                        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let headers = String::from_utf8_lossy(&request[..end]).to_string();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if request.len() < end + 4 + length {
                            continue;
                        }
                        let mut lines = headers.lines();
                        let mut first = lines.next().unwrap().split_whitespace();
                        break (
                            first.next().unwrap().to_string(),
                            first.next().unwrap().to_string(),
                            request[end + 4..end + 4 + length].to_vec(),
                        );
                    };
                    let response = {
                        let mut state = state.lock().unwrap();
                        let id = serde_json::from_slice::<Value>(&body)
                            .ok()
                            .and_then(|v| v.get("campaignId").and_then(|v| v.as_i64()))
                            .unwrap_or(0);
                        state.calls.push(format!("{method} {path} {id}"));
                        if path.starts_with(EP_DELIVERY) {
                            let activities = state
                                .activities
                                .iter()
                                .map(|item| {
                                    let mut item = item.clone();
                                    if state.claim_marks_claimed
                                        && item["claimable"] == Value::Bool(false)
                                        && item["status"].is_null()
                                    {
                                        item["status"] = Value::String("CLAIMED".to_string());
                                    }
                                    item
                                })
                                .collect::<Vec<_>>();
                            json!({"code": 0, "data": {"items": activities}})
                        } else if path == EP_CLAIM && state.fail_claim == Some(id) {
                            json!({"code": 409, "message": "活动暂不可领取"})
                        } else if path == EP_CONFIRM && state.fail_confirm == Some(id) {
                            json!({"code": 500, "message": "确认暂未成功"})
                        } else if path == EP_CLAIM || path == EP_CONFIRM {
                            let mark_claimed = path == EP_CONFIRM && state.fail_confirm != Some(id);
                            for item in &mut state.activities {
                                if item.get("campaignId").and_then(|v| v.as_i64()) == Some(id)
                                    && mark_claimed
                                {
                                    item["status"] = Value::String("CONFIRMED".to_string());
                                    item["claimable"] = Value::Bool(false);
                                }
                            }
                            json!({"code": 0, "data": {"campaignId": id}})
                        } else {
                            json!({"code": 404, "message": "not found"})
                        }
                    };
                    let body = response.to_string();
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    socket.write_all(body.as_bytes()).await.unwrap();
                });
            }
        });
        base
    }

    fn activity(id: i64, title: &str, amount: f64, claimed: bool) -> Value {
        json!({
            "campaignId": id, "type": "USER_LOGIN", "title": title,
            "benefitAmount": amount, "benefitUnit": "CREDIT",
            "status": claimed.then_some("CONFIRMED"), "claimable": !claimed,
        })
    }

    #[test]
    fn parses_delivery_payload() {
        let v = json!({
            "code": 0,
            "message": "ok",
            "data": {
                "channel": "IDE",
                "items": [
                    {"campaignId": 4, "title": "新用户注册送 4000 积分", "type": "NEW_USER_REGISTER",
                     "benefitAmount": 4000, "benefitUnit": "CREDIT", "status": "CONFIRMED", "claimable": false},
                    {"campaignId": 1, "title": "每日签到领积分", "type": "USER_LOGIN",
                     "benefitAmount": 1000, "benefitUnit": "CREDIT", "status": "CONFIRMED", "claimable": false}
                ]
            }
        });
        let acts = parse_activities(&v);
        assert_eq!(acts.len(), 2);
        let daily = acts.iter().find(|a| a.is_daily()).unwrap();
        assert_eq!(daily.campaign_id, 1);
        assert!(daily.claimed());
        assert_eq!(daily.benefit_amount, 1000.0);
    }

    #[test]
    fn detects_unclaimed_daily() {
        let v = json!({"data": {"items": [
            {"campaignId": 1, "title": "签到", "type": "USER_LOGIN", "benefitAmount": 1000, "status": null, "claimable": true}
        ]}});
        let acts = parse_activities(&v);
        assert_eq!(acts.len(), 1);
        assert!(!acts[0].claimed());
        assert!(acts[0].claimable);
    }

    #[test]
    fn daily_matched_by_type_when_id_differs() {
        let v =
            json!([{"campaignId": 99, "type": "daily_claim", "title": "t", "benefitAmount": 5}]);
        let acts = parse_activities(&v);
        assert!(acts[0].is_daily());
    }

    #[test]
    fn consumed_status_is_claimed() {
        let v = json!([{"campaignId": 1, "type": "USER_LOGIN", "status": "CONSUME"}]);
        assert!(parse_activities(&v)[0].claimed());
    }

    #[tokio::test]
    async fn claims_all_pending_daily_activities() {
        let state = Arc::new(Mutex::new(FakeOps {
            activities: vec![
                activity(42, "Skill创新赛报名礼", 2000.0, true),
                activity(40, "Skill创新赛每日签到礼", 1000.0, false),
                activity(34, "每日签到领100积分", 100.0, false),
            ],
            ..Default::default()
        }));
        let base = fake_server(state.clone()).await;
        let out = run_at(&reqwest::Client::new(), &base, "test-ak", "test-sk", None).await;

        assert!(out.ok, "{}", out.message);
        assert!(out.claimed_today);
        assert_eq!(out.benefit_amount, 1100.0);
        let calls = state.lock().unwrap().calls.join("\n");
        assert!(calls.contains("POST /v1/ops/claim 40"));
        assert!(calls.contains("POST /v1/ops/confirm 40"));
        assert!(calls.contains("POST /v1/ops/claim 34"));
        assert!(calls.contains("POST /v1/ops/confirm 34"));
        assert!(!calls.contains("claim 42"));
        assert_eq!(calls.matches("GET /v1/ops/delivery").count(), 2);
    }

    #[tokio::test]
    async fn failed_daily_does_not_block_other_activities() {
        let state = Arc::new(Mutex::new(FakeOps {
            activities: vec![
                activity(42, "Skill创新赛报名礼", 2000.0, true),
                activity(40, "Skill创新赛每日签到礼", 1000.0, false),
                activity(34, "每日签到领100积分", 100.0, false),
            ],
            fail_claim: Some(40),
            ..Default::default()
        }));
        let base = fake_server(state.clone()).await;
        let out = run_at(&reqwest::Client::new(), &base, "test-ak", "test-sk", None).await;

        assert!(!out.ok);
        assert!(!out.claimed_today);
        assert!(out.message.contains("Skill创新赛每日签到礼"));
        let calls = state.lock().unwrap().calls.join("\n");
        assert!(calls.contains("POST /v1/ops/claim 34"));
        assert!(calls.contains("POST /v1/ops/confirm 34"));
    }

    #[tokio::test]
    async fn confirmed_activities_do_not_trigger_another_claim() {
        let state = Arc::new(Mutex::new(FakeOps {
            activities: vec![
                activity(42, "Skill创新赛报名礼", 2000.0, true),
                activity(40, "Skill创新赛每日签到礼", 1000.0, true),
                activity(34, "每日签到领100积分", 100.0, true),
            ],
            ..Default::default()
        }));
        let base = fake_server(state.clone()).await;
        let out = run_at(&reqwest::Client::new(), &base, "test-ak", "test-sk", None).await;

        assert!(out.ok);
        assert!(out.claimed_today);
        assert_eq!(state.lock().unwrap().calls.len(), 1);
    }

    #[tokio::test]
    async fn pending_confirmation_is_not_reported_as_received() {
        let state = Arc::new(Mutex::new(FakeOps {
            activities: vec![activity(40, "Skill创新赛每日签到礼", 1000.0, false)],
            claim_marks_claimed: true,
            fail_confirm: Some(40),
            ..Default::default()
        }));
        let base = fake_server(state).await;
        let out = run_at(&reqwest::Client::new(), &base, "test-ak", "test-sk", None).await;

        assert!(!out.ok);
        assert!(!out.claimed_today);
        assert!(out.message.contains("未到账"));
    }
}
