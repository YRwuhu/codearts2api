//! 本地用量统计。
//!
//! 代理每处理完一次请求就记录一条：模型、账号、耗时、输入/输出 token、
//! 流式与否、成功与否。数据只落在内存 + 可选 JSONL 追加文件，
//! 用于 WebUI 的用量页和趋势图。
//!
//! 说明：CodeArts 上游不返回 prompt cache 命中字段，因此这里能统计的是
//! 「请求数 / token / 耗时 / 成功率」，不做缓存命中率——那是估算而非实测。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// 内存里最多保留的明细条数，防止长时间运行占用过多内存。
const MAX_RECORDS: usize = 20_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    pub ts: i64,
    pub model: String,
    pub account: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub reasoning_tokens: i64,
    pub elapsed_ms: i64,
    pub stream: bool,
    pub ok: bool,
    /// 失败时的错误码或简述
    #[serde(default)]
    pub error: Option<String>,
}

impl UsageRecord {
    pub fn from_upstream(
        model: &str,
        account: &str,
        elapsed_ms: i64,
        stream: bool,
        usage: Option<&Value>,
    ) -> Self {
        let prompt = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let completion = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let total = usage
            .and_then(|u| u.get("total_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(prompt + completion);
        let reasoning = usage
            .and_then(|u| u.get("completion_tokens_details"))
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        Self {
            ts: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            account: account.to_string(),
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            reasoning_tokens: reasoning,
            elapsed_ms,
            stream,
            ok: true,
            error: None,
        }
    }

    pub fn failed(model: &str, account: &str, elapsed_ms: i64, error: impl Into<String>) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp(),
            model: model.to_string(),
            account: account.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            reasoning_tokens: 0,
            elapsed_ms,
            stream: false,
            ok: false,
            error: Some(error.into()),
        }
    }
}

#[derive(Default, Serialize)]
struct Agg {
    requests: i64,
    ok_requests: i64,
    failed_requests: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    reasoning_tokens: i64,
    elapsed_ms_sum: i64,
}

impl Agg {
    fn add(&mut self, r: &UsageRecord) {
        self.requests += 1;
        if r.ok {
            self.ok_requests += 1;
        } else {
            self.failed_requests += 1;
        }
        self.prompt_tokens += r.prompt_tokens;
        self.completion_tokens += r.completion_tokens;
        self.total_tokens += r.total_tokens;
        self.reasoning_tokens += r.reasoning_tokens;
        self.elapsed_ms_sum += r.elapsed_ms;
    }

    fn to_json(&self) -> Value {
        let avg = if self.requests > 0 {
            self.elapsed_ms_sum as f64 / self.requests as f64
        } else {
            0.0
        };
        json!({
            "requests": self.requests,
            "ok_requests": self.ok_requests,
            "failed_requests": self.failed_requests,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.total_tokens,
            "reasoning_tokens": self.reasoning_tokens,
            "avg_elapsed_ms": avg.round() as i64,
        })
    }
}

pub struct UsageStore {
    records: Arc<Mutex<VecDeque<UsageRecord>>>,
    archive_path: Option<PathBuf>,
    started_at: i64,
}

impl UsageStore {
    pub fn new(archive_path: Option<PathBuf>) -> Self {
        Self {
            records: Arc::new(Mutex::new(VecDeque::with_capacity(1024))),
            archive_path,
            started_at: chrono::Utc::now().timestamp(),
        }
    }

    pub async fn record(&self, record: UsageRecord) {
        // 追加到 JSONL（失败不影响主流程）
        if let Some(path) = &self.archive_path {
            if let Ok(line) = serde_json::to_string(&record) {
                if let Some(dir) = path.parent() {
                    let _ = tokio::fs::create_dir_all(dir).await;
                }
                use tokio::io::AsyncWriteExt;
                if let Ok(mut f) = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .await
                {
                    let _ = f.write_all(format!("{line}\n").as_bytes()).await;
                }
            }
        }

        let mut guard = self.records.lock().await;
        if guard.len() >= MAX_RECORDS {
            guard.pop_front();
        }
        guard.push_back(record);
    }

    pub async fn len(&self) -> usize {
        self.records.lock().await.len()
    }

    /// 汇总：总体 + 按模型 + 按账号 + 时间序列。
    /// `since_ts` 为 0 表示全部时间。
    pub async fn summarize(&self, since_ts: i64, bucket: &str) -> Value {
        let guard = self.records.lock().await;
        let filtered: Vec<&UsageRecord> = guard
            .iter()
            .filter(|r| r.ts >= since_ts)
            .collect();

        let mut total = Agg::default();
        let mut by_model: BTreeMap<String, Agg> = BTreeMap::new();
        let mut by_account: BTreeMap<String, Agg> = BTreeMap::new();
        let mut by_bucket: BTreeMap<i64, Agg> = BTreeMap::new();

        let bucket_secs: i64 = match bucket {
            "hour" => 3600,
            "day" => 86400,
            _ => 3600,
        };

        for r in &filtered {
            total.add(r);
            by_model.entry(r.model.clone()).or_default().add(r);
            by_account.entry(r.account.clone()).or_default().add(r);
            let key = (r.ts / bucket_secs) * bucket_secs;
            by_bucket.entry(key).or_default().add(r);
        }

        let mut models: Vec<Value> = by_model
            .into_iter()
            .map(|(name, agg)| {
                let mut v = agg.to_json();
                v["model"] = json!(name);
                v
            })
            .collect();
        models.sort_by_key(|v| {
            -v.get("total_tokens")
                .and_then(|x| x.as_i64())
                .unwrap_or(0)
        });

        let mut accounts: Vec<Value> = by_account
            .into_iter()
            .map(|(name, agg)| {
                let mut v = agg.to_json();
                v["account"] = json!(name);
                v
            })
            .collect();
        accounts.sort_by_key(|v| {
            -v.get("total_tokens")
                .and_then(|x| x.as_i64())
                .unwrap_or(0)
        });

        let series: Vec<Value> = by_bucket
            .into_iter()
            .map(|(ts, agg)| {
                let mut v = agg.to_json();
                v["ts"] = json!(ts);
                v
            })
            .collect();

        json!({
            "summary": total.to_json(),
            "by_model": models,
            "by_account": accounts,
            "series": series,
            "bucket": bucket,
            "since": since_ts,
            "now": chrono::Utc::now().timestamp(),
            "started_at": self.started_at,
            "retained": guard.len(),
        })
    }

    /// 最近的请求明细（倒序）。
    pub async fn recent(&self, limit: usize, since_ts: i64) -> Vec<UsageRecord> {
        let guard = self.records.lock().await;
        guard
            .iter()
            .rev()
            .filter(|r| r.ts >= since_ts)
            .take(limit.min(500))
            .cloned()
            .collect()
    }

    /// 每个模型的最近一次请求时间与累计请求数，供 WebUI 标注活跃模型。
    pub async fn activity_by_model(&self) -> HashMap<String, (i64, i64)> {
        let guard = self.records.lock().await;
        let mut out: HashMap<String, (i64, i64)> = HashMap::new();
        for r in guard.iter().rev() {
            let entry = out.entry(r.model.clone()).or_insert((r.ts, 0));
            entry.1 += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_record_from_usage() {
        let usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "completion_tokens_details": {"reasoning_tokens": 20}
        });
        let r = UsageRecord::from_upstream("glm-5.3", "main", 1234, true, Some(&usage));
        assert_eq!(r.prompt_tokens, 100);
        assert_eq!(r.completion_tokens, 50);
        assert_eq!(r.total_tokens, 150);
        assert_eq!(r.reasoning_tokens, 20);
        assert!(r.ok);
    }

    #[test]
    fn missing_usage_defaults_to_zero() {
        let r = UsageRecord::from_upstream("m", "a", 10, false, None);
        assert_eq!(r.total_tokens, 0);
        assert!(r.ok);
    }

    #[tokio::test]
    async fn summarize_groups_by_model_and_account() {
        let store = UsageStore::new(None);
        for i in 0..3 {
            let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
            store
                .record(UsageRecord::from_upstream("m1", "a1", 100 + i, false, Some(&usage)))
                .await;
        }
        let s = store.summarize(0, "hour").await;
        assert_eq!(s["summary"]["requests"], 3);
        assert_eq!(s["summary"]["total_tokens"], 45);
        assert_eq!(s["by_model"][0]["model"], "m1");
        assert_eq!(s["by_account"][0]["account"], "a1");
    }
}
