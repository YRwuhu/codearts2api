use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 账号凭证来源。
#[derive(Clone, Debug, PartialEq)]
pub enum CredentialKind {
    /// 手动填写的永久 AK/SK
    Manual,
    /// OAuth 登录获得的 STS 临时凭证
    OAuth,
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub ak: String,
    pub sk: String,
    /// STS 安全令牌；OAuth 登录的账号才有，签名时需带 X-Security-Token
    pub security_token: Option<String>,
    pub kind: CredentialKind,
    pub enabled: bool,
}

impl Account {
    /// 构造手动 AK/SK 账号。
    pub fn manual(id: String, ak: String, sk: String, enabled: bool) -> Self {
        Self {
            id,
            ak,
            sk,
            security_token: None,
            kind: CredentialKind::Manual,
            enabled,
        }
    }

    pub fn is_oauth(&self) -> bool {
        self.kind == CredentialKind::OAuth
    }

    /// 签名用的 security_token
    pub fn security_token(&self) -> Option<&str> {
        self.security_token.as_deref()
    }
}

impl Account {
    /// 日志展示用，避免把完整 AK 打出来。
    pub fn masked_ak(&self) -> String {
        let ak = &self.ak;
        if ak.len() <= 8 {
            return "****".to_string();
        }
        format!("{}****{}", &ak[..4], &ak[ak.len() - 2..])
    }

    /// 缓存文件用：只保留字母数字和短横线，避免路径穿越。
    pub fn cache_slug(&self) -> String {
        self.id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub host: String,
    pub port: u16,
    /// 模型允许的最大输出 token 数兜底值（模型元数据缺失时使用）。
    pub max_tokens_cap: i64,
    /// 免费额度网关拿不到模型时，是否仍暴露兜底列表。
    pub benefit_fallback: bool,
    /// 每账号每日 token 上限；0 表示不限制。
    /// 借鉴 workbuddy2api 的 daily token cap：单账号可以设小一点防止一天把
    /// 套餐额度跑光，多账号则各算各的。
    pub daily_token_cap: i64,
    pub model_cache_path: PathBuf,
    pub env_dir: PathBuf,
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

    // 从当前目录向上找，兼容在子目录里运行
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

#[derive(Deserialize)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Deserialize)]
struct AccountEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    ak: String,
    #[serde(default)]
    sk: String,
    /// OAuth 账号：凭证来自 auths/ 目录，这里只需 id
    #[serde(default)]
    oauth: bool,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

fn unique_id(base: &str, used: &mut Vec<String>) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "account" } else { base };
    let mut id = base.to_string();
    let mut n = 2;
    while used.iter().any(|u| u == &id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    used.push(id.clone());
    id
}

fn load_accounts_file(path: &Path) -> Result<Vec<Account>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取账号文件 {} 失败: {e}", path.display()))?;
    let parsed: AccountsFile = serde_json::from_str(&text)
        .map_err(|e| format!("解析账号文件 {} 失败: {e}", path.display()))?;

    let mut used: Vec<String> = Vec::new();
    let mut accounts: Vec<Account> = Vec::new();
    for (idx, entry) in parsed.accounts.into_iter().enumerate() {
        let ak = entry.ak.trim().to_string();
        let sk = entry.sk.trim().to_string();
        let id = unique_id(&entry.id, &mut used);

        if entry.oauth {
            // OAuth 账号的凭证从 auths/ 读取，这里只占位
            accounts.push(Account {
                id,
                ak: String::new(),
                sk: String::new(),
                security_token: None,
                kind: CredentialKind::OAuth,
                enabled: entry.enabled,
            });
            continue;
        }

        if ak.is_empty() || sk.is_empty() {
            return Err(format!(
                "账号文件 {} 中第 {} 个账号缺少 ak 或 sk",
                path.display(),
                idx + 1
            ));
        }
        accounts.push(Account::manual(id, ak, sk, entry.enabled));
    }
    Ok(accounts)
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let env_file = locate_env_file();
        if let Some(path) = env_file.as_ref() {
            // 已存在的环境变量优先，不被 .env 覆盖
            let _ = dotenvy::from_path(path);
        }
        let env_dir = env_file
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // 账号来源优先级：CODEARTS_ACCOUNTS 指定文件 > 同目录 accounts.json
        // > .env 里的单个 AK/SK（向后兼容）。
        let accounts_path = std::env::var("CODEARTS_ACCOUNTS")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                let p = env_dir.join("accounts.json");
                if p.is_file() {
                    Some(p)
                } else {
                    None
                }
            });

        let mut accounts = match accounts_path {
            Some(path) => load_accounts_file(&path)?,
            None => Vec::new(),
        };

        // accounts.json 为空或不存在时回退到 .env 单账号
        if accounts.is_empty() {
            let ak = std::env::var("CODEARTS_AK").unwrap_or_default().trim().to_string();
            let sk = std::env::var("CODEARTS_SK").unwrap_or_default().trim().to_string();
            if !ak.is_empty() && !sk.is_empty() {
                accounts.push(Account::manual("default".to_string(), ak, sk, true));
            }
        }

        if accounts.is_empty() {
            return Err(
                "未配置任何账号：请在 .env 填写 CODEARTS_AK / CODEARTS_SK，\
                 或创建 accounts.json（参考 accounts.example.json）"
                    .to_string(),
            );
        }
        if !accounts.iter().any(|a| a.enabled) {
            return Err("所有账号都被禁用（enabled=false），没有可用账号".to_string());
        }

        let host = std::env::var("CODEARTS_PROXY_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string())
            .trim()
            .to_string();
        let port = std::env::var("CODEARTS_PROXY_PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(8787);

        // 兜底值只在上游没给元数据时用；真实硬上限是 params::UPSTREAM_MAX_OUTPUT_TOKENS。
        let max_tokens_cap = std::env::var("CODEARTS_MAX_TOKENS_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(crate::params::UPSTREAM_MAX_OUTPUT_TOKENS);

        let benefit_fallback = std::env::var("CODEARTS_BENEFIT_FALLBACK")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let daily_token_cap = std::env::var("CODEARTS_DAILY_TOKEN_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(0);

        let model_cache_path = std::env::var("CODEARTS_MODEL_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| env_dir.join("models-cache.json"));

        Ok(Config {
            accounts,
            host,
            port,
            max_tokens_cap,
            benefit_fallback,
            daily_token_cap,
            model_cache_path,
            env_dir,
        })
    }

    /// 单账号时沿用旧缓存名，多账号时按 id 区分，避免互相覆盖。
    pub fn cache_path_for(&self, account: &Account) -> PathBuf {
        if self.accounts.len() <= 1 {
            return self.model_cache_path.clone();
        }
        let parent = self
            .model_cache_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.env_dir.clone());
        let stem = self
            .model_cache_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("models-cache");
        parent.join(format!("{stem}.{}.json", account.cache_slug()))
    }
}
