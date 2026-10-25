//! 华为云 CodeArts 额度查询。
//!
//! 数据源是 snap-manager 的统计接口，返回套餐 credit 与 token 消耗。
//! 注意：CodeArts 采用订阅套餐 + credit 计费，**没有**签到/领权益类接口，
//! opengw 的 benefit 接口只对已开通免费额度的账号有效（本账号未开通）。
//! 因此这里只做只读的额度展示，不伪造"签到"动作。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::signer::Signer;

pub const STATISTICS_URL: &str =
    "https://snap-access.cn-north-4.myhuaweicloud.com/snap-manager/v1/statistics/plugin";

/// 华为云控制台的几个常用跳转地址，供 WebUI 直接展示。
pub fn console_links() -> Value {
    json!([
        {
            "name": "套餐用量与购买",
            "url": "https://console.huaweicloud.com/devcloud/?locale=zh-cn&region=cn-north-4#/subscribe/packageBuy?packageType=codearts_doer_credits",
            "desc": "查看 CodeArts 套餐额度、购买或续费"
        },
        {
            "name": "个人用量明细",
            "url": "https://codearts.huaweicloud.com/portal/settings/personal-usage",
            "desc": "按时间查看 token 与 credit 消耗明细"
        },
        {
            "name": "访问密钥管理 (AK/SK)",
            "url": "https://console.huaweicloud.com/iam/#/mine/accessKey",
            "desc": "创建或轮换 AK/SK，用于添加账号"
        },
        {
            "name": "CodeArts 智能体控制台",
            "url": "https://console.huaweicloud.com/codeartside/?region=cn-north-4#/codeartside/licenses",
            "desc": "管理订阅与许可"
        },
        {
            "name": "计费说明文档",
            "url": "https://support.huaweicloud.com/price-codeartsagent/codeartsagent_billing_0032.html",
            "desc": "官方计费规则（credit 如何换算 token）"
        }
    ])
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct CreditMetric {
    pub name: String,
    pub amount: f64,
    pub used: f64,
    pub remain: f64,
    pub expiring: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ModelMetric {
    pub model_id: String,
    pub model_name: String,
    pub description: String,
    pub usage_token: i64,
    pub enabled: bool,
    pub status: String,
    pub luxury: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Quota {
    pub ok: bool,
    pub error: Option<String>,
    /// 总 credit：总额 / 已用 / 剩余
    pub total_amount: f64,
    pub total_used: f64,
    pub total_remain: f64,
    pub expiring_amount: f64,
    /// 已消耗 token
    pub used_tokens: i64,
    /// 分套餐明细（基础包、赠送包、按需包）
    pub packages: Vec<CreditMetric>,
    pub models: Vec<ModelMetric>,
    pub fetched_at: i64,
}

fn num(v: Option<&Value>) -> f64 {
    v.and_then(|x| {
        x.as_f64()
            .or_else(|| x.as_str().and_then(|s| s.parse::<f64>().ok()))
    })
    .unwrap_or(0.0)
}

fn int(v: Option<&Value>) -> i64 {
    v.and_then(|x| {
        x.as_i64()
            .or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok()))
    })
    .unwrap_or(0)
}

/// 从统计接口的原始 JSON 中解析额度信息。
pub fn parse_quota(value: &Value) -> Quota {
    let mut quota = Quota {
        ok: true,
        fetched_at: chrono::Utc::now().timestamp(),
        ..Default::default()
    };

    if let Some(metrics) = value.get("metrics").and_then(|m| m.as_array()) {
        for metric in metrics {
            let name = metric
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            match name {
                // 已消耗 token 总量
                "usageTokenChatMessages" => {
                    quota.used_tokens = int(metric.get("usage_token_num"));
                }
                // 套餐总 credit
                "usageTotalPackageCredit" => {
                    quota.total_amount = num(metric.get("package_credit_amount"));
                    quota.total_used = num(metric.get("package_credit_used"));
                    quota.total_remain = num(metric.get("package_credit_remain"));
                    quota.expiring_amount =
                        num(metric.get("package_credit_expiring_amount"));
                }
                // 分套餐明细
                "usageBasicPackageCredit" => quota.packages.push(CreditMetric {
                    name: "基础套餐".to_string(),
                    amount: num(metric.get("package_credit_amount")),
                    used: num(metric.get("package_credit_used")),
                    remain: num(metric.get("package_credit_remain")),
                    expiring: num(metric.get("package_credit_expiring_amount")),
                }),
                "usageBonusPackageCredit" => quota.packages.push(CreditMetric {
                    name: "赠送额度".to_string(),
                    amount: num(metric.get("package_credit_amount")),
                    used: num(metric.get("package_credit_used")),
                    remain: num(metric.get("package_credit_remain")),
                    expiring: num(metric.get("package_credit_expiring_amount")),
                }),
                "usageOnDemandPackageCredit" => {
                    let amount = num(metric.get("package_credit_amount"));
                    // 按需套餐通常为 0，无意义时不展示
                    if amount > 0.0 {
                        quota.packages.push(CreditMetric {
                            name: "按需付费".to_string(),
                            amount,
                            used: num(metric.get("package_credit_used")),
                            remain: num(metric.get("package_credit_remain")),
                            expiring: num(metric.get("package_credit_expiring_amount")),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    if let Some(models) = value.get("model_metrics").and_then(|m| m.as_array()) {
        for model in models {
            quota.models.push(ModelMetric {
                model_id: model
                    .get("model_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                model_name: model
                    .get("model_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                description: model
                    .get("model_desc")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                usage_token: int(model.get("usage_token_num")),
                enabled: model.get("enable").and_then(|v| v.as_bool()).unwrap_or(false),
                status: model
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                luxury: model
                    .get("is_luxury")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
    }

    quota
}

/// 用某个账号的凭证拉取额度。
pub async fn fetch_quota(
    http: &reqwest::Client,
    ak: &str,
    sk: &str,
    security_token: Option<&str>,
) -> Quota {
    if ak.trim().is_empty() || sk.trim().is_empty() {
        return Quota {
            ok: false,
            error: Some("缺少 AK/SK 凭证（OAuth 账号请先登录）".to_string()),
            fetched_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
    }

    let signer = Signer::new(ak, sk);
    // OAuth 账号用 STS 临时凭证，必须走完整签名（带 X-Security-Token）
    let signed = if security_token.is_some() {
        signer.sign_full(
            "GET",
            STATISTICS_URL,
            b"",
            &[
                ("content-type", "application/json"),
                ("x-language", "zh-cn"),
                ("accept", "application/json"),
            ],
            security_token,
        )
    } else {
        signer.sign(
            "GET",
            STATISTICS_URL,
            b"",
            &[("x-language", "zh-cn"), ("accept", "application/json")],
        )
    };

    let mut req = http.get(STATISTICS_URL);
    for (k, v) in signed.headers {
        req = req.header(k, v);
    }

    let resp = match req.timeout(std::time::Duration::from_secs(30)).send().await {
        Ok(r) => r,
        Err(e) => {
            return Quota {
                ok: false,
                error: Some(format!("请求失败: {e}")),
                fetched_at: chrono::Utc::now().timestamp(),
                ..Default::default()
            }
        }
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Quota {
            ok: false,
            error: Some(format!("HTTP {status}: {}", &text[..text.len().min(200)])),
            fetched_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
    }

    match serde_json::from_str::<Value>(&text) {
        Ok(value) => {
            if let Some(msg) = value.get("error_msg").and_then(|v| v.as_str()) {
                return Quota {
                    ok: false,
                    error: Some(msg.to_string()),
                    fetched_at: chrono::Utc::now().timestamp(),
                    ..Default::default()
                };
            }
            parse_quota(&value)
        }
        Err(e) => Quota {
            ok: false,
            error: Some(format!("解析响应失败: {e}")),
            fetched_at: chrono::Utc::now().timestamp(),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_credit_and_tokens() {
        let raw = json!({
            "metrics": [
                {"name": "usageTokenChatMessages", "usage_token_num": 4266511},
                {
                    "name": "usageTotalPackageCredit",
                    "package_credit_amount": 6500.0,
                    "package_credit_used": 52.83,
                    "package_credit_remain": 6447.17,
                    "package_credit_expiring_amount": 0
                },
                {
                    "name": "usageBasicPackageCredit",
                    "package_credit_amount": 500.0,
                    "package_credit_used": 0.0,
                    "package_credit_remain": 500.0
                }
            ],
            "model_metrics": [
                {"model_id": "GLM-5.2", "model_name": "GLM-5.2", "enable": true, "status": "normal"}
            ]
        });
        let q = parse_quota(&raw);
        assert!(q.ok);
        assert_eq!(q.used_tokens, 4_266_511);
        assert!((q.total_amount - 6500.0).abs() < 0.01);
        assert!((q.total_remain - 6447.17).abs() < 0.01);
        assert_eq!(q.packages.len(), 1);
        assert_eq!(q.models.len(), 1);
        assert_eq!(q.models[0].model_id, "GLM-5.2");
    }

    #[test]
    fn skips_zero_on_demand_package() {
        let raw = json!({
            "metrics": [
                {"name": "usageOnDemandPackageCredit", "package_credit_amount": 0}
            ]
        });
        let q = parse_quota(&raw);
        assert!(q.packages.is_empty());
    }
}
