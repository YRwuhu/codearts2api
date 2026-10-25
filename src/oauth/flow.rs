//! OAuth 登录的 HTTP 侧：发起会话、本地回调、轮询完成。
//!
//! 登录采用"浏览器打开授权页 + 双通道取凭据"：
//!   主通道（ticket 轮询）：浏览器登录后，用 ticket_id + secret 轮询
//!     snap-manager /v1/login/ticket 取回 STS 凭证。这条通道不依赖回调，
//!     在 WebUI 场景下最可靠。
//!   可选通道（本地回调）：portal 若回调到 127.0.0.1，收到 code 后走
//!     authorization_code 换 token。

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::oauth::{self, Sessions};
use crate::web::WebState;

/// 登录会话表（进程级单例，够用：登录是低频人工操作）。
pub fn sessions() -> &'static Sessions {
    use std::sync::OnceLock;
    static CELL: OnceLock<Sessions> = OnceLock::new();
    CELL.get_or_init(Sessions::new)
}

fn random_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn open_browser(url: &str) -> bool {
    #[cfg(windows)]
    let child = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let child = std::process::Command::new("xdg-open").arg(url).spawn();

    child.is_ok()
}

/// POST /api/oauth/start —— 创建登录会话，返回授权链接。
pub async fn start(State(state): State<WebState>) -> Response {
    let port = state.config().port;

    let ticket_id = random_id();
    let secret = random_id();
    let (verifier, challenge) = oauth::pkce();
    let dpop = oauth::DpopKey::generate();

    let auth_url = oauth::build_authorize_url(&ticket_id, &challenge, port);
    let session_id = random_id();

    sessions()
        .put(
            session_id.clone(),
            oauth::Session {
                ticket_id,
                secret,
                verifier,
                dpop,
                port,
                auth_url: auth_url.clone(),
                created_at: chrono::Utc::now().timestamp(),
                done: false,
                error: None,
                identity: None,
            },
        )
        .await;

    let opened = open_browser(&auth_url);
    println!("[oauth] 登录会话 {session_id} 已创建（自动打开浏览器={opened}）");

    Json(json!({
        "ok": true,
        "session_id": session_id,
        "auth_url": auth_url,
        "opened": opened,
    }))
    .into_response()
}

/// GET /oauth/callback —— portal 回调。
///
/// portal 会回调两次：第一阶段带 secret + redirect（要求 307 跳回），
/// 第二阶段带 code。第一阶段若报错会中断整条链路。
#[derive(Deserialize, Default)]
pub struct CallbackParams {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub redirect: Option<String>,
    #[serde(default)]
    pub ticket_id: Option<String>,
}

pub async fn callback(
    State(state): State<WebState>,
    Query(p): Query<CallbackParams>,
) -> Response {
    let code = p.code.unwrap_or_default();
    let secret = p.secret.unwrap_or_default();
    let redirect = p.redirect.unwrap_or_default();
    let ticket_id = p.ticket_id.unwrap_or_default();

    if !secret.is_empty() && !ticket_id.is_empty() {
        sessions().set_secret_by_ticket(&ticket_id, &secret).await;
    }

    if code.is_empty() && !redirect.is_empty() {
        // 第一阶段：按 portal 要求在 307 跳回指定地址
        let mut resp = Response::new(axum::body::Body::empty());
        *resp.status_mut() = StatusCode::TEMPORARY_REDIRECT;
        if let Ok(loc) = redirect.parse() {
            resp.headers_mut()
                .insert(axum::http::header::LOCATION, loc);
        }
        return resp;
    }

    if code.is_empty() {
        return Html("<h3>登录进行中…回调缺少 code</h3>").into_response();
    }

    // 第二阶段：有 code，找到对应会话完成换取
    match complete_with_code(&state, &ticket_id, &code).await {
        Ok(name) => {
            Html(format!("<h3>登录成功，可以关闭此页面。</h3><p>账号：{name}</p>")).into_response()
        }
        Err(e) => Html(format!("<h3>登录失败</h3><p>{e}</p>")).into_response(),
    }
}

async fn complete_with_code(
    state: &WebState,
    ticket_id: &str,
    code: &str,
) -> Result<String, String> {
    // 回调 URL 可能不带 ticket_id，此时退化为"最近创建且未完成的会话"
    let found = if ticket_id.is_empty() {
        sessions().latest_pending().await
    } else {
        sessions().find_by_ticket(ticket_id).await
    };

    let Some((_ticket, _secret, verifier, dpop, port)) = found else {
        return Err("找不到对应的登录会话（可能已过期），请重新发起登录".to_string());
    };

    let mut identity = oauth::exchange_code(&state.http, code, &verifier, port, &dpop).await?;

    // 登录响应有时不带 user_id/user_name，用 STS 凭证补查一次
    if identity.user_id.is_empty() {
        match oauth::fetch_current_user(&state.http, &identity).await {
            Ok((uid, uname, domain)) => {
                if !uid.is_empty() {
                    identity.user_id = uid;
                }
                if !uname.is_empty() {
                    identity.user_name = uname;
                }
                if !domain.is_empty() {
                    identity.domain_id = domain;
                }
            }
            Err(e) => eprintln!("[oauth] 补查身份失败（不影响凭证）: {e}"),
        }
    }

    let name = identity.display_name();

    // 关键：落盘 + 登记账号
    crate::admin::save_oauth_identity(state, identity.clone()).await?;

    // 找到 session_id 并标记完成
    if let Some((session_id, ..)) = sessions().latest_pending().await {
        sessions().mark_done(&session_id, identity).await;
    }

    println!("[oauth] 登录成功（回调通道）: {name}");
    Ok(name)
}

/// POST /api/oauth/poll —— 轮询登录结果。
pub async fn poll(State(state): State<WebState>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Req {
        session_id: String,
    }
    let req: Req = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("参数错误: {e}") })),
            )
                .into_response()
        }
    };

    let Some(info) = sessions().info(&req.session_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "登录会话不存在或已过期，请重新发起登录" })),
        )
            .into_response();
    };

    if let Some(err) = info.error {
        if !err.is_empty() {
            return Json(json!({ "status": "error", "error": err })).into_response();
        }
    }
    if info.done {
        return Json(json!({ "status": "done", "user_name": info.user_name })).into_response();
    }

    if let Some((ticket_id, secret, verifier, dpop, _)) =
        sessions().find_by_ticket_by_session(&req.session_id).await
    {
        match oauth::poll_ticket(&state.http, &ticket_id, &secret, &verifier, &dpop).await {
            Ok(Some(identity)) => {
                let name = identity.display_name();
                if let Err(e) = crate::admin::save_oauth_identity(&state, identity.clone()).await {
                    let msg = format!("保存账号失败: {e}");
                    sessions().mark_failed(&req.session_id, msg.clone()).await;
                    return Json(json!({ "status": "error", "error": msg })).into_response();
                }
                sessions().mark_done(&req.session_id, identity).await;
                println!("[oauth] 登录成功（ticket 通道）: {name}");
                return Json(json!({ "status": "done", "user_name": name })).into_response();
            }
            Ok(None) => {}
            Err(e) => println!("[oauth] ticket 轮询: {e}"),
        }
    }

    Json(json!({ "status": "pending" })).into_response()
}
