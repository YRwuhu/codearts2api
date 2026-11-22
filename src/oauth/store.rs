//! OAuth 凭证的持久化与自动续期。
//!
//! 凭证按账号存放在 `auths/codearts-{user_id}.json`（权限 0600），
//! 与 accounts.json 解耦：accounts.json 只记录"这个账号用 OAuth"，
//! 真正敏感的 STS 凭证、refresh_token、DPoP 私钥都在 auths/ 下。

use std::path::{Path, PathBuf};

use crate::oauth::Identity;

fn auth_dirs(env_dir: &Path) -> Vec<PathBuf> {
    #[cfg(target_os = "android")]
    {
        mobile_auth_dirs(env_dir)
    }
    #[cfg(not(target_os = "android"))]
    vec![std::env::var("CODEARTS_AUTH_DIR")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| env_dir.join("auths"))]
}

#[cfg(any(target_os = "android", test))]
fn mobile_auth_dirs(env_dir: &Path) -> Vec<PathBuf> {
    let canonical = env_dir.join("auths");
    let legacy = env_dir.join("auth");
    if !canonical.is_dir() && legacy.is_dir() {
        vec![legacy, canonical]
    } else {
        vec![canonical, legacy]
    }
}

fn credential_path(dirs: &[PathBuf], user_id: &str) -> PathBuf {
    let name = file_name(user_id);
    dirs.iter()
        .map(|dir| dir.join(&name))
        .find(|path| path.exists())
        .unwrap_or_else(|| dirs[0].join(name))
}

fn file_name(user_id: &str) -> String {
    let safe: String = user_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    };
    format!("codearts-{safe}.json")
}

/// 保存（原子写，0600）。
pub fn save(env_dir: &Path, identity: &Identity) -> Result<PathBuf, String> {
    let path = credential_path(&auth_dirs(env_dir), &identity.user_id);
    let dir = path.parent().expect("credential path has a directory");
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(identity).map_err(|e| format!("序列化失败: {e}"))?;

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
    let path = credential_path(&auth_dirs(env_dir), user_id);
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// 列出全部 OAuth 凭证。
pub fn load_all(env_dir: &Path) -> Vec<Identity> {
    let dirs = auth_dirs(env_dir);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let filename = name.to_string_lossy();
            if !filename.starts_with("codearts-")
                || !filename.ends_with(".json")
                || !seen.insert(name)
            {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Ok(id) =
                    serde_json::from_str::<Identity>(text.trim_start_matches('\u{feff}'))
                {
                    if id.is_complete() || !id.refresh_token.is_empty() {
                        out.push(id);
                    }
                }
            }
        }
    }
    out
}

pub fn remove(env_dir: &Path, user_id: &str) -> Result<(), String> {
    let name = file_name(user_id);
    let mut found = false;
    for dir in auth_dirs(env_dir) {
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => found = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(format!("删除失败: {e}")),
        }
    }
    if !found {
        return Err(format!("账号 {user_id} 的凭证不存在"));
    }
    Ok(())
}

/// 续期一个账号并写回。返回更新后的凭证。
pub async fn refresh_and_save(
    http: &reqwest::Client,
    env_dir: &Path,
    identity: &Identity,
) -> Result<Identity, String> {
    // Model calls, status checks and the scheduled job can hit the same expiry at once.
    // Refresh tokens may rotate, so reuse a credential that another task has just refreshed.
    static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = REFRESH.lock().await;
    if let Some(current) = load(env_dir, &identity.user_id) {
        if current.is_complete()
            && !current.expired()
            && (current.refresh_token != identity.refresh_token
                || current.access_key_id != identity.access_key_id
                || current.expiration != identity.expiration)
        {
            return Ok(current);
        }
    }
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
    fn mobile_finds_existing_legacy_credentials_alongside_canonical_ones() {
        let dir = std::env::temp_dir().join(format!("ca2a-auth-mobile-{}", rand::random::<u64>()));
        let canonical = dir.join("auths");
        let legacy = dir.join("auth");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir(&legacy).unwrap();
        let legacy_file = legacy.join(file_name("user"));
        std::fs::write(&legacy_file, "{}").unwrap();
        let dirs = mobile_auth_dirs(&dir);
        assert_eq!(credential_path(&dirs, "user"), legacy_file);
        assert_eq!(
            credential_path(&dirs, "new"),
            canonical.join(file_name("new"))
        );
        let canonical_file = canonical.join(file_name("user"));
        std::fs::write(&canonical_file, "{}").unwrap();
        assert_eq!(credential_path(&dirs, "user"), canonical_file);
        std::fs::remove_dir_all(dir).unwrap();
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
