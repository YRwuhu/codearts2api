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

    /// 派生账号级上游会话 ID 用的稳定种子。
    ///
    /// 必须**跨凭证续期稳定**，否则上游会把续期当成新会话，又回到
    /// `TM.00001041`。所以：
    ///
    /// - 手动 AK/SK 账号：用 AK（永不变）。
    /// - OAuth 账号：用 `id`（即 user_id），**不能**用 STS 临时 AK——
    ///   那个每 24 小时换一次。
    pub fn session_seed(&self) -> String {
        if self.is_oauth() {
            self.id.clone()
        } else {
            self.ak.clone()
        }
    }
}

impl Account {
    /// 是否还是 `.env.example` / `accounts.example.json` 里的占位符。
    ///
    /// 华为云 AK/SK 全是 ASCII 字母数字；出现非 ASCII 字符只可能是
    /// 用户照抄示例没替换。
    pub fn looks_like_placeholder(&self) -> bool {
        if self.is_oauth() {
            return false;
        }
        let suspicious = |s: &str| !s.is_ascii();
        suspicious(&self.ak) || suspicious(&self.sk)
    }

    /// 日志展示用，避免把完整 AK 打出来。
    ///
    /// 按**字符**而不是字节截取：`.env.example` 的占位符是中文，
    /// 按字节切片会落在 UTF-8 字符中间直接 panic。
    pub fn masked_ak(&self) -> String {
        let chars: Vec<char> = self.ak.chars().collect();
        if chars.len() <= 8 {
            return "****".to_string();
        }
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        format!("{head}****{tail}")
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

/// 单请求体默认上限（MB）。axum 自带的 2 MB 对于贴大段代码/日志的
/// 场景太小，这里放宽到 32 MB。
pub const DEFAULT_MAX_BODY_MB: usize = 32;
/// `CODEARTS_MAX_BODY_MB` 的封顶值，防止误配把内存吃满。
pub const MAX_BODY_MB_CEILING: usize = 512;

#[derive(Clone, Debug)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub host: String,
    pub port: u16,
    /// 本地在途流式请求上限，不是上游会话数。
    pub concurrency: usize,
    /// 模型允许的最大输出 token 数兜底值（模型元数据缺失时使用）。
    pub max_tokens_cap: i64,
    /// 免费额度网关拿不到模型时，是否仍暴露兜底列表。
    pub benefit_fallback: bool,
    /// 工具轮回合是否改用非流式上游请求（默认关）。
    ///
    /// 这是针对「上游 3 会话限制」的临时妥协：降级会丢掉工具调用的增量帧。
    /// 账号级会话头（`x-ot-session-id`）修好后该限制已消失，因此默认关闭；
    /// 只有在确实需要压低流式请求数时才打开。
    pub tool_turn_plain: bool,
    /// 流式槽位耗尽时是否降级为非流式上游请求。
    ///
    /// 默认 `false`：降级会破坏工具调用的增量帧，agent 类客户端体验明显变差。
    /// 需要"永不 429"时设 `CODEARTS_STREAM_DEGRADE=1`。
    pub stream_degrade: bool,
    /// 每账号每日 token 上限；0 表示不限制。
    /// 借鉴 workbuddy2api 的 daily token cap：单账号可以设小一点防止一天把
    /// 套餐额度跑光，多账号则各算各的。
    pub daily_token_cap: i64,
    /// 单个请求体的大小上限（字节）。
    ///
    /// axum 的默认值是 2 MB，而把一整份反编译代码或大文件贴进 prompt 很容易
    /// 超过去——那样请求会在本地就被 413 拦掉，根本到不了上游。
    pub max_body_bytes: usize,
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
            let ak = std::env::var("CODEARTS_AK")
                .unwrap_or_default()
                .trim()
                .to_string();
            let sk = std::env::var("CODEARTS_SK")
                .unwrap_or_default()
                .trim()
                .to_string();
            if !ak.is_empty() && !sk.is_empty() {
                accounts.push(Account::manual("default".to_string(), ak, sk, true));
            }
        }

        // 账号为空不算错误：首次使用需要先起服务，再从 WebUI 点
        // 「登录华为云」走 OAuth。CLI 子命令用 `require_accounts` 自行校验。
        let host = std::env::var("CODEARTS_PROXY_HOST")
            .unwrap_or_else(|_| "127.0.0.1".to_string())
            .trim()
            .to_string();
        let port = std::env::var("CODEARTS_PROXY_PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(8787);

        let concurrency = std::env::var("CODEARTS_CONCURRENCY")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(8);

        // 兜底值只在上游没给元数据时用；真实硬上限是 params::UPSTREAM_MAX_OUTPUT_TOKENS。
        let max_tokens_cap = std::env::var("CODEARTS_MAX_TOKENS_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(crate::params::UPSTREAM_MAX_OUTPUT_TOKENS);

        let benefit_fallback = std::env::var("CODEARTS_BENEFIT_FALLBACK")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        // 账号级会话头（x-ot-session-id）修好后，上游 3 会话限制已消失，
        // 工具轮不再需要降级非流式。保留开关供特殊场景使用。
        let tool_turn_plain = std::env::var("CODEARTS_TOOL_TURN_PLAIN")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let stream_degrade = std::env::var("CODEARTS_STREAM_DEGRADE")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        let daily_token_cap = std::env::var("CODEARTS_DAILY_TOKEN_CAP")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(0);

        // 上限设个封顶，避免手滑写个超大值把内存吃满。
        let max_body_bytes = std::env::var("CODEARTS_MAX_BODY_MB")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MAX_BODY_MB)
            .min(MAX_BODY_MB_CEILING)
            * 1024
            * 1024;

        let model_cache_path = std::env::var("CODEARTS_MODEL_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| env_dir.join("models-cache.json"));

        Ok(Config {
            accounts,
            host,
            port,
            concurrency,
            max_tokens_cap,
            benefit_fallback,
            tool_turn_plain,
            stream_degrade,
            daily_token_cap,
            max_body_bytes,
            model_cache_path,
            env_dir,
        })
    }

    /// CLI 子命令用：没有可用账号时直接报错，并给出下一步提示。
    ///
    /// 服务本身（`serve`）允许零账号启动，因为首次使用需要先打开 WebUI
    /// 走 OAuth 登录；而那些依赖账号的 CLI 命令没法这么做，只能报错。
    pub fn require_accounts(&self) -> Result<(), String> {
        if self.accounts.is_empty() {
            return Err("未配置任何账号：启动 serve 后打开 WebUI 点「登录华为云」，\
                 或在 .env 填写 CODEARTS_AK / CODEARTS_SK、\
                 创建 accounts.json（参考 accounts.example.json）"
                .to_string());
        }
        if !self.accounts.iter().any(|a| a.enabled) {
            return Err("所有账号都被禁用（enabled=false），没有可用账号".to_string());
        }
        let usable = self
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.looks_like_placeholder())
            .count();
        if usable == 0 {
            return Err("账号的 AK/SK 仍是示例里的占位符：请填入真实密钥，\
                 或改用 WebUI 的「登录华为云」"
                .to_string());
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_ak_handles_ascii() {
        let a = Account::manual("x".into(), "AKIDEXAMPLE1234".into(), "sk".into(), true);
        assert_eq!(a.masked_ak(), "AKID****34");
    }

    /// `.env.example` 里的中文占位符按字节切会 panic 在字符中间，
    /// 这正是 config.rs:71 那次崩溃。
    #[test]
    fn masked_ak_survives_multibyte_placeholder() {
        let a = Account::manual(
            "default".into(),
            "你的AccessKey".into(),
            "你的SecretKey".into(),
            true,
        );
        assert_eq!(a.masked_ak(), "你的Ac****ey");
    }

    #[test]
    fn masked_ak_short_values_are_fully_hidden() {
        let a = Account::manual("x".into(), "短的".into(), "sk".into(), true);
        assert_eq!(a.masked_ak(), "****");
    }

    #[test]
    fn placeholder_credentials_are_detected() {
        let a = Account::manual(
            "default".into(),
            "你的AccessKey".into(),
            "你的SecretKey".into(),
            true,
        );
        assert!(a.looks_like_placeholder());

        let real = Account::manual(
            "main".into(),
            "HPUAGWKFANXWYUGESDL2".into(),
            "aNDkSDwGZhz".into(),
            true,
        );
        assert!(!real.looks_like_placeholder());
    }

    #[test]
    fn oauth_accounts_are_never_placeholders() {
        let a = Account {
            id: "u1".into(),
            ak: String::new(),
            sk: String::new(),
            security_token: None,
            kind: CredentialKind::OAuth,
            enabled: true,
        };
        assert!(!a.looks_like_placeholder());
    }
}
