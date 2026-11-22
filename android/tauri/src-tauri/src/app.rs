use crate::gateway::{self, Access};
use codearts2api::{admin, api, config::Config, models::Pool, usage::UsageStore, web::WebState};
use std::{env, fs, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tauri::{Manager, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

#[tauri::mobile_entry_point]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let storage = env::var_os("EXTERNAL_STORAGE").unwrap_or_else(|| "/sdcard".into());
            let data_dir = std::path::PathBuf::from(storage).join("CodeArts");
            let private_dir = app.path().app_data_dir()?;
            match tauri::async_runtime::block_on(start(&data_dir, &private_dir)) {
                Ok(access) => {
                    let url: Url = format!("{}{}", access.origin, access.bootstrap).parse()?;
                    let handle = app.handle().clone();
                    WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                        .on_navigation(move |url| {
                            if url.scheme() == "http"
                                && url.host_str() == Some("127.0.0.1")
                                && url.port_or_known_default()
                                    == access.authority.rsplit_once(':').and_then(|(_, port)| port.parse().ok())
                                && url.username().is_empty()
                                && url.password().is_none()
                            {
                                return true;
                            }
                            if url.scheme() == "https"
                                && url.host_str() == Some("codearts.huaweicloud.com")
                                && url.port_or_known_default() == Some(443)
                                && url.path() == "/portal/authorize"
                                && url.username().is_empty()
                                && url.password().is_none()
                            {
                                if let Err(error) = handle.opener().open_url(url.as_str(), None::<String>) {
                                    eprintln!("[android] 无法打开授权页: {error}");
                                }
                            }
                            false
                        })
                        .build()?;
                }
                Err(error) => {
                    let message = serde_json::to_string(&error)?;
                    WebviewWindowBuilder::new(
                        app, "main", WebviewUrl::App("android-startup.html".into()),
                    )
                    .initialization_script(format!(
                        "window.addEventListener('DOMContentLoaded', () => {{ document.getElementById('problem').textContent = {message}; }});"
                    ))
                    .build()?;
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start CodeArts2API");
}

async fn start(data_dir: &Path, private_dir: &Path) -> Result<Arc<Access>, String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("无法访问 {}：{e}。请检查文件权限。", data_dir.display()))?;
    let mut config = Config::load_from_dir(data_dir)?;
    let ip = match config.host.as_str() {
        "127.0.0.1" | "localhost" => std::net::Ipv4Addr::LOCALHOST,
        "0.0.0.0" => std::net::Ipv4Addr::UNSPECIFIED,
        _ => return Err("Android 的 CODEARTS_PROXY_HOST 请使用 127.0.0.1 或 0.0.0.0".into()),
    };
    let listener = tokio::net::TcpListener::bind((ip, config.port))
        .await
        .map_err(|e| format!("无法监听 {}:{}：{e}", ip, config.port))?;
    config.host = ip.to_string();
    config.port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let authority = format!("127.0.0.1:{}", config.port);
    let access = Arc::new(Access {
        origin: format!("http://{authority}"),
        authority,
        bootstrap: format!("/_android/{}", gateway::random_secret()),
        session: gateway::random_secret(),
        api_key: api_key(private_dir)?,
        allow_lan: ip.is_unspecified(),
    });
    let certs = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| reqwest::Certificate::from_der(cert.as_ref()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("TLS 根证书加载失败：{e}"))?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .tls_certs_only(certs)
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;
    let usage = Arc::new(UsageStore::new(Some(data_dir.join("usage.jsonl"))));
    let state = WebState::new(config, Pool::new(Vec::new()), http, usage);
    let router = gateway::router(api::router(state.clone()), access.clone(), data_dir);
    tauri::async_runtime::spawn(async move {
        // Bind and render the console before waiting on any upstream account refresh.
        let initialize = async move {
            if let Err(e) = admin::rebuild_pool(&state).await {
                eprintln!("[android] {e}");
            }
            admin::spawn_quota_refresher(state.clone());
            admin::spawn_checkin_scheduler(state);
        };
        tokio::spawn(initialize);
        if let Err(e) = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            eprintln!("[android] 代理停止：{e}");
        }
    });
    Ok(access)
}

fn api_key(private_dir: &Path) -> Result<String, String> {
    fs::create_dir_all(private_dir).map_err(|e| e.to_string())?;
    let path = private_dir.join("proxy-key");
    match fs::read_to_string(&path) {
        Ok(value) if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(value),
        Ok(_) => Err("代理密钥文件无效，请清除应用数据后重试".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let value = gateway::random_secret();
            let temp = path.with_extension("tmp");
            fs::write(&temp, &value)
                .and_then(|_| fs::rename(temp, path))
                .map_err(|e| format!("保存代理密钥失败：{e}"))?;
            Ok(value)
        }
        Err(e) => Err(format!("读取代理密钥失败：{e}")),
    }
}
