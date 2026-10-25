//! 每日签到 —— 浏览器自动化方案。
//!
//! 为什么必须用浏览器：
//!   CodeArts 的签到接口位于 `devcloud.cn-north-4.huaweicloud.com/chat/...`，
//!   服务端会拦截非浏览器客户端（实测：无论补齐多少请求头，都返回
//!   HTTP 200 + 空响应体，属于 TLS/HTTP2 指纹级别的检测）。
//!   因此无法用 reqwest 直接调用，只能驱动真实浏览器。
//!
//! 实现方式：
//!   - `tools/pw_login.js`  打开有界面的浏览器，用户登录一次，
//!     cookie 持久化到 `tools/profile/`；
//!   - `tools/checkin-run.js` 复用该 profile，无头执行签到并输出 JSON。
//!
//! 两者都通过 `node` 子进程调用，后端只负责解析结果。

use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 签到结果，直接转发给前端。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Outcome {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub claimed_today: bool,
    #[serde(default)]
    pub activities: Vec<Value>,
    #[serde(default)]
    pub needs_login: bool,
    #[serde(default)]
    pub at: i64,
}

/// 工具目录：优先环境变量，其次项目根的 tools/。
pub fn tools_dir(env_dir: &std::path::Path) -> PathBuf {
    if let Ok(p) = std::env::var("CODEARTS_TOOLS_DIR") {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // env_dir 是 .env 所在目录（项目根）
    let candidate = env_dir.join("tools");
    if candidate.is_dir() {
        return candidate;
    }
    // 兜底：可执行文件同级的 tools/
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("tools");
            if p.is_dir() {
                return p;
            }
        }
    }
    candidate
}

fn profile_dir(tools: &std::path::Path) -> PathBuf {
    if let Ok(p) = std::env::var("CODEARTS_CHECKIN_PROFILE") {
        let p = p.trim();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    tools.join("profile")
}

fn node_cmd() -> String {
    std::env::var("CODEARTS_NODE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "node".to_string())
}

/// 检查运行环境是否齐备（node + playwright + 脚本）。
pub struct Readiness {
    pub ok: bool,
    pub node: bool,
    pub playwright: bool,
    pub script: bool,
    pub profile: bool,
    pub detail: String,
}

pub fn check_ready(env_dir: &std::path::Path) -> Readiness {
    let tools = tools_dir(env_dir);
    let node_path = node_cmd();
    let node_ok = which_exists(&node_path);
    let script = tools.join("checkin-run.js").is_file();
    let pw = tools.join("node_modules").join("playwright").is_dir();
    let profile = profile_dir(&tools).is_dir();

    let mut missing = Vec::new();
    if !node_ok {
        missing.push("node");
    }
    if !script {
        missing.push("tools/checkin-run.js");
    }
    if !pw {
        missing.push("tools/node_modules/playwright");
    }

    Readiness {
        ok: missing.is_empty(),
        node: node_ok,
        playwright: pw,
        script,
        profile,
        detail: if missing.is_empty() {
            let mut msg = "浏览器自动化环境就绪".to_string();
            if profile {
                msg.push_str("，已有登录态");
            }
            msg
        } else {
            format!(
                "缺少 {}；请在项目根执行 `cd tools && npm install playwright`",
                missing.join("、")
            )
        },
    }
}

fn which_exists(cmd: &str) -> bool {
    if std::path::Path::new(cmd).is_file() {
        return true;
    }
    // Windows 上 node 通常是 node.exe
    let exe = format!("{cmd}.exe");
    if std::path::Path::new(&exe).is_file() {
        return true;
    }
    // 交给系统 PATH 判断
    std::process::Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn parse_child_output(stdout: &str) -> Outcome {
    // 脚本最后一行是 JSON
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or("");

    match serde_json::from_str::<Outcome>(line) {
        Ok(mut o) => {
            o.at = chrono::Utc::now().timestamp();
            if o.message.is_empty() {
                o.message = if o.ok { "签到成功".into() } else { "签到失败".into() };
            }
            o
        }
        Err(_) => Outcome {
            ok: false,
            message: format!(
                "无法解析脚本输出: {}",
                stdout.chars().take(300).collect::<String>()
            ),
            at: chrono::Utc::now().timestamp(),
            ..Default::default()
        },
    }
}

async fn run_node(tools: &std::path::Path, script: &str, profile: &std::path::Path) -> Outcome {
    if !tools.join(script).is_file() {
        return Outcome {
            ok: false,
            message: format!("找不到脚本 {}", tools.join(script).display()),
            at: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
    }

    let mut cmd = tokio::process::Command::new(node_cmd());
    cmd.arg(script)
        .current_dir(tools)
        .env("CK_PROFILE", profile)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                ok: false,
                message: format!("无法启动 node: {e}"),
                at: chrono::Utc::now().timestamp(),
                ..Default::default()
            }
        }
    };

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(180),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Outcome {
                ok: false,
                message: format!("执行失败: {e}"),
                at: chrono::Utc::now().timestamp(),
                ..Default::default()
            }
        }
        Err(_) => {
            return Outcome {
                ok: false,
                message: "签到超时（超过 180 秒）".to_string(),
                at: chrono::Utc::now().timestamp(),
                ..Default::default()
            }
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut outcome = parse_child_output(&stdout);
    if !outcome.ok && outcome.message.starts_with("无法解析") && !stderr.trim().is_empty() {
        outcome.message = format!(
            "{}（stderr: {}）",
            outcome.message,
            stderr.chars().take(200).collect::<String>()
        );
    }
    outcome
}

/// 执行一次签到。
pub async fn run_checkin(env_dir: &std::path::Path) -> Outcome {
    let tools = tools_dir(env_dir);
    let profile = profile_dir(&tools);
    run_node(&tools, "checkin-run.js", &profile).await
}

/// 查询签到状态（不执行领取）。
pub async fn fetch_status(env_dir: &std::path::Path) -> Outcome {
    let tools = tools_dir(env_dir);
    let profile = profile_dir(&tools);
    run_node(&tools, "checkin-status.js", &profile).await
}

/// 启动交互式登录（有界面浏览器）。这是一个长时间运行的前台进程，
/// 由后端一次性触发，前端轮询 profile 状态判断是否完成。
pub async fn start_login(env_dir: &std::path::Path) -> Result<(), String> {
    let tools = tools_dir(env_dir);
    let script = tools.join("pw_login.js");
    if !script.is_file() {
        return Err(format!("找不到登录脚本 {}", script.display()));
    }
    let profile = profile_dir(&tools);

    let mut cmd = tokio::process::Command::new(node_cmd());
    cmd.arg("pw_login.js")
        .current_dir(&tools)
        .env("CK_PROFILE", &profile)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());

    #[cfg(windows)]
    {
        // 独立进程组，避免被父进程的信号影响
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("启动登录窗口失败: {e}"))
}
