mod admin;
mod api;
mod checkin;
mod config;
mod models;
mod oauth;
mod params;
mod quota;
mod signer;
mod stream;
mod usage;
mod web;

use std::process::ExitCode;
use std::sync::Arc;

use config::{Account, Config};
use models::{Catalog, Pool};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("serve");

    match command {
        "serve" | "" => serve(),
        "accounts" => accounts_cmd(),
        "benefit" => benefit_cmd(&args[2..]),
        "selftest" => selftest_cmd(&args[2..]),
        "models" => models_cmd(),
        "-h" | "--help" | "help" => {
            print_help();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("未知命令: {other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        r#"CodeArts2API (Rust) - 华为云 CodeArts 转 OpenAI 兼容代理

用法:
  codearts2api serve            启动 HTTP 代理（默认）
  codearts2api accounts         列出已配置账号及其模型数量
  codearts2api models           列出合并后的可用模型
  codearts2api selftest [MODEL...]  对运行中的代理做端到端自检
  codearts2api benefit [status|claim|models|balance] [账号id]
                                查询/领取免费额度

多账号:
  在 .env 同目录创建 accounts.json（参考 accounts.example.json）即可启用多账号。
  请求按文件中的顺序尝试，遇到并发超限/鉴权失败/该账号无此模型时自动切换下一个。
  未创建 accounts.json 时退回 .env 里的单个 CODEARTS_AK / CODEARTS_SK。

环境变量（也可写在 .env）:
  CODEARTS_AK / CODEARTS_SK     华为云访问密钥（单账号模式）
  CODEARTS_ACCOUNTS             账号文件路径，默认 .env 同目录 accounts.json
  CODEARTS_PROXY_HOST           监听地址，默认 127.0.0.1
  CODEARTS_PROXY_PORT           监听端口，默认 8787
  CODEARTS_MAX_TOKENS_CAP       max_tokens 兜底上限，默认 65536
  CODEARTS_BENEFIT_FALLBACK     1 = 免费模型拿不到时仍暴露兜底列表
  CODEARTS_MODEL_CACHE          模型缓存路径，默认与 .env 同目录
"#
    );
}

fn build_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("创建运行时失败: {e}"))
}

/// 按配置构建每个启用账号的模型目录。
async fn build_pool(cfg: &Config, http: reqwest::Client) -> Pool {
    let enabled: Vec<&Account> = cfg.accounts.iter().filter(|a| a.enabled).collect();
    let mut catalogs = Vec::new();
    for account in enabled {
        let cache_path = cfg.cache_path_for(account);
        let catalog = Catalog::new(
            account.clone(),
            &cache_path,
            cfg.benefit_fallback,
            http.clone(),
        );
        // 启动时同步一次真实模型列表；失败则回退到缓存/兜底
        if !catalog.refresh().await {
            catalog.load_cache().await;
        }
        catalogs.push(Arc::new(catalog));
    }
    Pool::new(catalogs)
}

fn serve() -> ExitCode {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let rt = match build_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    rt.block_on(async move {
        let http = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("创建 HTTP 客户端失败: {e}");
                return ExitCode::FAILURE;
            }
        };

        let pool = build_pool(&cfg, http.clone()).await;
        let merged: Vec<String> = pool
            .merged_models()
            .await
            .iter()
            .map(|m| m.id.clone())
            .collect();

        let addr = format!("{}:{}", cfg.host, cfg.port);
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("无法监听 {addr}: {e}");
                eprintln!("（端口被占用时请先停止已有实例，或修改 CODEARTS_PROXY_PORT）");
                return ExitCode::FAILURE;
            }
        };

        let account_count = pool.catalogs().len();
        println!("CodeArts OpenAI 兼容代理已启动: http://{addr}");
        println!("OpenAI Base URL: http://{addr}/v1");
        println!("账号数: {account_count}，合并可用模型 {} 个: {merged:?}", merged.len());
        if account_count > 1 {
            println!("多账号已启用：失败时按配置顺序自动切换");
        }
        println!("管理界面: http://{addr}/");
        println!("按 Ctrl+C 停止");

        let archive = cfg.env_dir.join("usage.jsonl");
        let usage = std::sync::Arc::new(usage::UsageStore::new(Some(archive)));
        let state = web::WebState::new(cfg.clone(), pool, http, usage);

        // 后台定时任务：定期刷新额度并落盘，供 WebUI 与 /api/quota 使用
        admin::spawn_quota_refresher(state.clone());
        admin::spawn_checkin_scheduler(state.clone());

        let app = api::router(state);
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("服务异常退出: {e}");
            return ExitCode::FAILURE;
        }
        ExitCode::SUCCESS
    })
}

fn accounts_cmd() -> ExitCode {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = match build_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let http = reqwest::Client::new();
        let pool = build_pool(&cfg, http).await;
        let described = pool.describe().await;
        println!("{:<18} {:<18} {:<8} {}", "账号", "AK", "状态", "模型数");
        println!("{}", "-".repeat(60));
        for account in &cfg.accounts {
            if !account.enabled {
                println!("{:<18} {:<18} {:<8} -", account.id, account.masked_ak(), "disabled");
                continue;
            }
            let count = described
                .iter()
                .find(|(id, _, _)| id == &account.id)
                .map(|(_, _, n)| *n)
                .unwrap_or(0);
            println!(
                "{:<18} {:<18} {:<8} {}",
                account.id,
                account.masked_ak(),
                "enabled",
                count
            );
        }

        let merged = pool.merged_models().await;
        println!("\n合并去重后共 {} 个模型:", merged.len());
        for m in merged {
            let owners: Vec<String> = {
                let mut v = Vec::new();
                for c in pool.catalogs() {
                    if c.contains(&m.id).await {
                        v.push(c.account_id().to_string());
                    }
                }
                v
            };
            println!("  {:<28} 可用账号: {}", m.id, owners.join(", "));
        }
        ExitCode::SUCCESS
    })
}

fn models_cmd() -> ExitCode {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = match build_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let http = reqwest::Client::new();
        let pool = build_pool(&cfg, http).await;
        for m in pool.merged_models().await {
            let ctx = m
                .context_window
                .map(|v| format!("{v}"))
                .unwrap_or_else(|| "?".to_string());
            let max = m
                .max_tokens
                .map(|v| format!("{v}"))
                .unwrap_or_else(|| "?".to_string());
            println!("{:<28} {:<8} ctx={:<9} max={}", m.id, m.channel, ctx, max);
        }
        ExitCode::SUCCESS
    })
}

fn benefit_cmd(args: &[String]) -> ExitCode {
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // 可选第二个参数指定账号；未指定时用第一个启用账号
    let target = args
        .get(1)
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty());
    let account = match target {
        Some(id) => cfg.accounts.iter().find(|a| a.id == id),
        None => cfg.accounts.iter().find(|a| a.enabled),
    };
    let account = match account {
        Some(a) => a.clone(),
        None => {
            eprintln!("找不到指定账号: {}", target.unwrap_or("(第一个启用账号)"));
            return ExitCode::FAILURE;
        }
    };

    let rt = match build_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async move {
        let http = reqwest::Client::new();
        let base = models::OPENGW_BASE;
        let action = args.first().map(|s| s.as_str()).unwrap_or("default");

        let call = |method: &'static str, path: String| {
            let ak = account.ak.clone();
            let sk = account.sk.clone();
            let http = http.clone();
            async move {
                let url = format!("{base}{path}");
                let signer = signer::Signer::new(&ak, &sk);
                let signed = signer.sign(method, &url, b"", &[("x-language", "zh-cn")]);
                let mut req = http.request(method.parse().unwrap_or(reqwest::Method::GET), &url);
                for (k, v) in signed.headers {
                    req = req.header(k, v);
                }
                let resp = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
                let text = resp.text().await.unwrap_or_default();
                let value: serde_json::Value = serde_json::from_str(&text)
                    .unwrap_or_else(|_| serde_json::json!({ "raw": text }));
                let code = value
                    .get("error_code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0000")
                    .to_string();
                if code != "0000" {
                    return Err(format!(
                        "业务错误 {code}: {}",
                        value
                            .get("error_msg")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                    ));
                }
                Ok(value
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})))
            }
        };

        println!("账号: {}", account.id);
        let result: Result<(), String> = match action {
            "status" => call("GET", "/api/v1/benefit/claim".to_string())
                .await
                .map(|r| println!("最近签到时间: {}", fmt_claim_time(&r))),
            "claim" => call("POST", "/api/v1/benefit/claim".to_string())
                .await
                .map(|r| println!("签到成功，最近签到时间: {}", fmt_claim_time(&r))),
            "balance" => call("GET", "/api/v1/user/tokens/balance".to_string())
                .await
                .map(|r| {
                    let total = r.get("total_quota").and_then(|v| v.as_i64()).unwrap_or(0);
                    let remain = r.get("total_balance").and_then(|v| v.as_i64()).unwrap_or(0);
                    let used = r.get("used_amount").and_then(|v| v.as_i64()).unwrap_or(0);
                    println!("免费额度余额: {remain} / {total}（已用 {used}）");
                }),
            "models" => call("GET", "/api/v1/gateway/config".to_string())
                .await
                .map(|r| {
                    println!("免费模型列表:");
                    if let Some(models) = r.get("models").and_then(|m| m.as_array()) {
                        for m in models {
                            let id = m.get("model_id").and_then(|v| v.as_str()).unwrap_or("?");
                            println!("  {id}");
                        }
                    }
                }),
            _ => async {
                match call("GET", "/api/v1/benefit/claim".to_string()).await {
                    Ok(r) => {
                        println!("最近签到时间: {}", fmt_claim_time(&r));
                        match call("POST", "/api/v1/benefit/claim".to_string()).await {
                            Ok(r2) => {
                                println!("签到成功，最近签到时间: {}", fmt_claim_time(&r2));
                                match call("GET", "/api/v1/user/tokens/balance".to_string()).await {
                                    Ok(b) => {
                                        let total = b
                                            .get("total_quota")
                                            .and_then(|v| v.as_i64())
                                            .unwrap_or(0);
                                        let remain = b
                                            .get("total_balance")
                                            .and_then(|v| v.as_i64())
                                            .unwrap_or(0);
                                        println!("免费额度余额: {remain} / {total}");
                                        Ok(())
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            .await,
        };

        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                println!("失败: {e}");
                ExitCode::FAILURE
            }
        }
    })
}

fn fmt_claim_time(result: &serde_json::Value) -> String {
    match result.get("create_time").and_then(|v| v.as_i64()) {
        Some(ms) if ms > 0 => {
            let secs = ms / 1000;
            match chrono::DateTime::from_timestamp(secs, 0) {
                Some(dt) => dt
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string(),
                None => "未知".to_string(),
            }
        }
        _ => "从未签到".to_string(),
    }
}

fn selftest_cmd(args: &[String]) -> ExitCode {
    let rt = match build_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let models: Vec<String> = if args.is_empty() {
        vec!["deepseek-v4.1-flash".to_string()]
    } else {
        args.to_vec()
    };

    rt.block_on(async move {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        let base = std::env::var("CODEARTS_SELFTEST_BASE")
            .unwrap_or_else(|_| "http://127.0.0.1:8787/v1".to_string());

        let mut all_ok = true;
        for model in models {
            let payload = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "只回复两个字：成功"}],
                "stream": false,
                "max_tokens": 2048
            });
            match http
                .post(format!("{base}/chat/completions"))
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(v) if v.get("choices").is_some() => {
                            let content =
                                v["choices"][0]["message"]["content"].as_str().unwrap_or("");
                            let finish = v["choices"][0]["finish_reason"].as_str().unwrap_or("");
                            println!("[非流式] {model}: finish={finish} content={content:?}");
                        }
                        _ => {
                            println!("[非流式] {model}: HTTP {status} {text}");
                            all_ok = false;
                        }
                    }
                }
                Err(e) => {
                    println!("[非流式] {model}: 请求失败 {e}");
                    all_ok = false;
                }
            }

            let payload = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "数到三，用中文"}],
                "stream": true,
                "max_tokens": 2048
            });
            match http
                .post(format!("{base}/chat/completions"))
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => {
                    let text = resp.text().await.unwrap_or_default();
                    let chunks = text.matches("data:").count();
                    let done = text.contains("[DONE]");
                    let has_error = text.contains("\"error\"");
                    let mut content = String::new();
                    for line in text.lines() {
                        let Some(p) = line.strip_prefix("data:") else {
                            continue;
                        };
                        let p = p.trim();
                        if p.is_empty() || p == "[DONE]" {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                            if let Some(s) = v["choices"][0]["delta"]["content"].as_str() {
                                content.push_str(s);
                            }
                        }
                    }
                    println!(
                        "[流式] {model}: chunks={chunks} done={done} error={has_error} content={content:?}"
                    );
                    if !done || has_error || content.is_empty() {
                        all_ok = false;
                    }
                }
                Err(e) => {
                    println!("[流式] {model}: 请求失败 {e}");
                    all_ok = false;
                }
            }
        }

        println!("RESULT: {}", if all_ok { "PASS" } else { "FAIL" });
        if all_ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    })
}
