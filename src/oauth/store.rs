//! OAuth 凭证的持久化与自动续期。
//!
//! 凭证按账号存放在 `auths/codearts-{user_id}.json`（权限 0600），
//! 与 accounts.json 解耦：accounts.json 只记录"这个账号用 OAuth"，
//! 真正敏感的 STS 凭证、refresh_token、DPoP 私钥都在 auths/ 下。

use std::path::{Path, PathBuf};

use crate::oauth::Identity;

pub fn auths_dir(env_dir: &Path) -> PathBuf {
    std::env::var("CODEARTS_AUTH_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| env_dir.join("auths"))
}

fn file_name(user_id: &str) -> String {
    let safe: String = user_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let safe = if safe.is_empty() { "unknown".to_string() } else { safe };
    format!("codearts-{safe}.json")
}

/// 保存（原子写，0600）。
pub fn save(env_dir: &Path, identity: &Identity) -> Result<PathBuf, String> {
    let dir = auths_dir(env_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;

    let path = dir.join(file_name(&identity.user_id));
    let text = serde_json::to_string_pretty(identity)
        .map_err(|e| format!("序列化失败: {e}"))?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写入失败: {e}"))?;

    // 收紧权限（Windows 上尽力而为）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, &path).map_err(|e| format!("重命名失败: {e}"))?;
    Ok(path)
}

/// 读取某个账号的凭证。
pub fn load(env_dir: &Path, user_id: &str) -> Option<Identity> {
    let path = auths_dir(env_dir).join(file_name(user_id));
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 列出全部 OAuth 凭证。
pub fn load_all(env_dir: &Path) -> Vec<Identity> {
    let dir = auths_dir(env_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("codearts-") || !name.ends_with(".json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if let Ok(id) = serde_json::from_str::<Identity>(&text) {
                if id.is_complete() || !id.refresh_token.is_empty() {
                    out.push(id);
                }
            }
        }
    }
    out
}

pub fn remove(env_dir: &Path, user_id: &str) -> Result<(), String> {
    let path = auths_dir(env_dir).join(file_name(user_id));
    if !path.is_file() {
        return Err(format!("账号 {user_id} 的凭证不存在"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("删除失败: {e}"))
}

/// 续期一个账号并写回。返回更新后的凭证。
pub async fn refresh_and_save(
    http: &reqwest::Client,
    env_dir: &Path,
    identity: &Identity,
) -> Result<Identity, String> {
    let updated = crate::oauth::refresh(http, identity).await?;
    save(env_dir, &updated)?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_is_sanitised() {
        assert_eq!(file_name("abc123"), "codearts-abc123.json");
        // 路径分隔符必须被替换，避免目录穿越
        assert_eq!(file_name("../etc/passwd"), "codearts-___etc_passwd.json");
        assert_eq!(file_name(""), "codearts-unknown.json");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ca2a-auth-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let id = Identity {
            user_id: "u123".into(),
            user_name: "tester".into(),
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            security_token: "TOK".into(),
            expiration: "2030-01-01T00:00:00Z".into(),
            ..Default::default()
        };

        save(&dir, &id).unwrap();
        let back = load(&dir, "u123").unwrap();
        assert_eq!(back.user_name, "tester");
        assert_eq!(back.access_key_id, "AK");
        assert!(back.is_complete());

        assert_eq!(load_all(&dir).len(), 1);
        remove(&dir, "u123").unwrap();
        assert!(load(&dir, "u123").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
