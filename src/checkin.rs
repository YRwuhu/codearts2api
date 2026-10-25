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
}

impl<'a> Ctx<'a> {
    async fn call(
        &self,
        method: &str,
        path_query: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let url = format!("{SNAP_BASE}{path_query}");
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
    let ctx = Ctx {
        http,
        ak,
        sk,
        security_token,
    };
    let value = ctx
        .call("GET", &format!("{EP_DELIVERY}?channel={CHANNEL}"), None)
        .await?;
    Ok(parse_activities(&value))
}

/// 执行签到：查询 → 领取 → 确认。
pub async fn run(
    http: &reqwest::Client,
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
    };

    let activities = match fetch_activities(http, ak, sk, security_token).await {
        Ok(a) => a,
        Err(e) => {
            out.message = e;
            return out;
        }
    };
    out.activities = activities.clone();

    let Some(daily) = activities.iter().find(|a| a.is_daily()).cloned() else {
        out.message = "未找到每日签到活动".to_string();
        return out;
    };
    out.benefit_amount = daily.benefit_amount;

    if daily.claimed() {
        out.ok = true;
        out.claimed_today = true;
        out.message = "今日已签到".to_string();
        return out;
    }

    // 领取
    let idem = format!(
        "claim_{}_{}",
        daily.campaign_id,
        chrono::Utc::now().timestamp_millis()
    );
    let claim_body = json!({
        "campaignId": daily.campaign_id,
        "channel": CHANNEL,
        "idempotentKey": idem,
    });
    if let Err(e) = ctx.call("POST", EP_CLAIM, Some(claim_body)).await {
        // 未到刷新时间 / 已领取都算今天已完成
        let benign = e.contains("尚未到达") || e.contains("已领取");
        out.ok = benign;
        out.claimed_today = benign;
        out.message = if benign {
            format!("今日已签到（{e}）")
        } else {
            format!("领取失败: {e}")
        };
        return out;
    }

    // 确认
    let confirm_ok = ctx
        .call(
            "POST",
            EP_CONFIRM,
            Some(json!({ "campaignId": daily.campaign_id })),
        )
        .await
        .is_ok();

    out.ok = true;
    out.claimed_today = true;
    out.message = if confirm_ok {
        if daily.benefit_amount > 0.0 {
            format!("签到成功，获得 {:.0} 积分", daily.benefit_amount)
        } else {
            "签到成功".to_string()
        }
    } else {
        "领取成功，确认步骤稍后自动到账".to_string()
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let v = json!([{"campaignId": 99, "type": "daily_claim", "title": "t", "benefitAmount": 5}]);
        let acts = parse_activities(&v);
        assert!(acts[0].is_daily());
    }

    #[test]
    fn consumed_status_is_claimed() {
        let v = json!([{"campaignId": 1, "type": "USER_LOGIN", "status": "CONSUME"}]);
        assert!(parse_activities(&v)[0].claimed());
    }
}
