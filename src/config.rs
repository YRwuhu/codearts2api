use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub ak: String,
    pub sk: String,
    pub host: String,
    pub port: u16,
    /// 模型允许的最大输出 token 数兜底值（模型元数据缺失时使用）。
    pub max_tokens_cap: i64,
    /// 免费额度网关拿不到模型时，是否仍暴露兜底列表。
    pub benefit_fallback: bool,
    pub model_cache_path: PathBuf,
}

fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Ok(p) = std::env::var("CODEARTS_ENV_FILE") {
        out.push(PathBuf::from(p));
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    out.push(cwd.join(".env"));

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join(".env"));
        }
    }

    // 从当前目录向上找，兼容在子目录里运行（如 rust/ 下 cargo run）
    let mut dir = cwd.as_path();
    for _ in 0..4 {
        if let Some(parent) = dir.parent() {
            out.push(parent.join(".env"));
            dir = parent;
        } else {
            break;
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..5 {
            let Some(d) = dir.clone() else { break };
            out.push(d.join(".env"));
            dir = d.parent().map(Path::to_path_buf);
        }
    }

    out
}

fn locate_env_file() -> Option<PathBuf> {
    candidates().into_iter().find(|p| p.is_file())
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let env_file = locate_env_file();
        if let Some(path) = env_file.as_ref() {
            // 已存在的环境变量优先，不被 .env 覆盖
            let _ = dotenvy::from_path(path);
        }

        let ak = std::env::var("CODEARTS_AK").unwrap_or_default().trim().to_string();
        let sk = std::env::var("CODEARTS_SK").unwrap_or_default().trim().to_string();
        if ak.is_empty() || sk.is_empty() {
            return Err("缺少 CODEARTS_AK / CODEARTS_SK，请在 .env 中配置后重试".to_string());
        }

        let host = std::env::var("CODEARTS_PROXY_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string())
            .trim()
            .to_string();
        let port = std::env::var("CODEARTS_PROXY_PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(8787);

        let max_tokens_cap = std::env::var("CODEARTS_MAX_TOKENS_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(65536);

        let benefit_fallback = std::env::var("CODEARTS_BENEFIT_FALLBACK")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let env_dir = env_file
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let model_cache_path = std::env::var("CODEARTS_MODEL_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| env_dir.join("models-cache.json"));

        Ok(Config {
            ak,
            sk,
            host,
            port,
            max_tokens_cap,
            benefit_fallback,
            model_cache_path,
        })
    }
}
