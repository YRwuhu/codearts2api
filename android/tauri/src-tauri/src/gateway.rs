use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;
use std::{net::SocketAddr, path::Path, sync::Arc};

pub(crate) struct Access {
    pub(crate) authority: String,
    pub(crate) origin: String,
    pub(crate) bootstrap: String,
    pub(crate) session: String,
    pub(crate) api_key: String,
    pub(crate) allow_lan: bool,
}

#[cfg(target_os = "android")]
pub(crate) fn random_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub(crate) fn router(shared: Router, access: Arc<Access>, data_dir: &Path) -> Router {
    let settings = json!({
        "base_url": format!("{}/v1", access.origin),
        "api_key": access.api_key,
        "data_dir": data_dir.display().to_string(),
        "allow_lan": access.allow_lan,
    });
    shared
        .route("/api/mobile", get(move || async move { Json(settings) }))
        .layer(middleware::from_fn_with_state(access, guard))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

async fn guard(State(access): State<Arc<Access>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let headers = request.headers();
    if path.starts_with("/v1/") {
        let bearer = header_value(headers, header::AUTHORIZATION.as_str())
            .and_then(|value| value.strip_prefix("Bearer "));
        if request.method() == Method::OPTIONS
            || bearer == Some(access.api_key.as_str())
            || header_value(headers, "x-api-key") == Some(access.api_key.as_str())
        {
            return next.run(request).await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"Invalid API key","type":"authentication_error"}})),
        )
            .into_response();
    }

    let local_host = header_value(headers, "host") == Some(access.authority.as_str())
        && request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|peer| peer.0.ip().is_loopback());
    if !local_host {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request.method() == Method::GET && path == "/oauth/callback" {
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; frame-ancestors 'none'"
                .parse()
                .unwrap(),
        );
        response
            .headers_mut()
            .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
        return response;
    }
    if request.method() == Method::GET && path == access.bootstrap {
        let mut response = Redirect::to("/").into_response();
        let cookie = format!(
            "codearts_admin={}; Path=/; SameSite=Strict; HttpOnly",
            access.session
        );
        response
            .headers_mut()
            .insert(header::SET_COOKIE, cookie.parse().unwrap());
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
            .headers_mut()
            .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
        return response;
    }

    let session = header_value(headers, "cookie").is_some_and(|cookie| {
        cookie.split(';').any(|part| {
            part.trim().strip_prefix("codearts_admin=") == Some(access.session.as_str())
        })
    });
    let same_origin = header_value(headers, "origin").is_none_or(|origin| origin == access.origin);
    if !session || !same_origin {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; font-src 'self' https://cdn.jsdelivr.net; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap(),
    );
    if path_is_private(response.headers()) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

fn path_is_private(headers: &HeaderMap) -> bool {
    header_value(headers, header::CONTENT_TYPE.as_str())
        .is_some_and(|kind| kind.starts_with("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use codearts2api::{api, config::Config, models::Pool, usage::UsageStore, web::WebState};
    use tower::ServiceExt;

    #[tokio::test]
    async fn management_and_proxy_credentials_are_separate() {
        let data_dir =
            std::env::temp_dir().join(format!("codearts-gateway-{}", rand::random::<u64>()));
        let state = WebState::new(
            Config::load_from_dir(&data_dir).unwrap(),
            Pool::new(Vec::new()),
            reqwest::Client::new(),
            Arc::new(UsageStore::new(None)),
        );
        let app = router(
            api::router(state),
            Arc::new(Access {
                authority: "127.0.0.1:8787".into(),
                origin: "http://127.0.0.1:8787".into(),
                bootstrap: "/_android/start-token".into(),
                session: "session-token".into(),
                api_key: "client-token".into(),
                allow_lan: false,
            }),
            &data_dir,
        );
        let call = |path: &str, host: &str, extra: &[(&str, &str)]| {
            let mut request =
                Request::builder()
                    .uri(path)
                    .header("host", host)
                    .extension(ConnectInfo(
                        "127.0.0.1:12345".parse::<SocketAddr>().unwrap(),
                    ));
            for (name, value) in extra {
                request = request.header(*name, *value);
            }
            app.clone().oneshot(request.body(Body::empty()).unwrap())
        };
        let local = "127.0.0.1:8787";
        let cookie = [("cookie", "codearts_admin=session-token")];
        let bearer = [("authorization", "Bearer client-token")];
        assert_eq!(
            call("/api/mobile", local, &bearer).await.unwrap().status(),
            401
        );
        assert_eq!(
            call("/api/mobile", local, &cookie).await.unwrap().status(),
            200
        );
        assert_eq!(
            call("/api/mobile", "192.168.0.8:8787", &cookie)
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            call(
                "/api/mobile",
                local,
                &[cookie[0], ("origin", "https://other.site")]
            )
            .await
            .unwrap()
            .status(),
            401
        );
        assert_eq!(
            call("/v1/models", local, &cookie).await.unwrap().status(),
            401
        );
        assert_eq!(
            call("/v1/models", local, &bearer).await.unwrap().status(),
            200
        );
        let models = call("/v1/models", local, &bearer).await.unwrap();
        let body = to_bytes(models.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"],
            json!([])
        );
        assert_eq!(
            call("/v1/models", local, &[("x-api-key", "client-token")])
                .await
                .unwrap()
                .status(),
            200
        );
        let start = call("/_android/start-token", local, &[]).await.unwrap();
        assert_eq!(start.status(), 303);
        assert!(start.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("HttpOnly"));
        for path in ["/", "/accounts"] {
            let page = call(path, local, &cookie).await.unwrap();
            assert_eq!(page.status(), 200);
            let body = to_bytes(page.into_body(), 128 * 1024).await.unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(html.contains("data-platform=\"android\""));
            assert!(html.contains("/app.js"));
        }
    }
}
