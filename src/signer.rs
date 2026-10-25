//! 华为云 SDK-HMAC-SHA256 请求签名。
//!
//! 与 Python 版保持一致的两点关键行为：
//! 1. 参与签名的头名一律小写，并按字典序拼接；
//! 2. 规范 URI 必须以 `/` 结尾（CodeArts 的 API 网关要求）。

use hmac::{Hmac, Mac};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// RFC 3986 unreserved 之外全部编码：`A-Z a-z 0-9 - _ . ~`
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode(value: &str) -> String {
    utf8_percent_encode(value, ENCODE_SET).to_string()
}

/// 供查询参数拼装复用（与签名规范保持同一套转义规则）。
pub fn encode_component(value: &str) -> String {
    encode(value)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn sdk_date() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn hmac_hex(key: &[u8], data: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 把 URL 拆成规则要求的三段：canonical_uri / canonical_query / netloc
fn split_url(url: &str) -> (String, String, String) {
    let parsed = match url::Url::parse(url) {
        Ok(v) => v,
        Err(_) => return ("/".to_string(), String::new(), String::new()),
    };
    let host = match parsed.port() {
        Some(port) => format!("{}:{}", parsed.host_str().unwrap_or(""), port),
        None => parsed.host_str().unwrap_or("").to_string(),
    };

    let path = parsed.path();
    let canonical_uri = if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    };

    let mut params: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    params.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let canonical_query = params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    (canonical_uri, canonical_query, host)
}

pub struct Signer<'a> {
    pub ak: &'a str,
    pub sk: &'a str,
}

pub struct SignedRequest {
    pub headers: Vec<(String, String)>,
}

impl<'a> Signer<'a> {
    pub fn new(ak: &'a str, sk: &'a str) -> Self {
        Self { ak, sk }
    }

    /// 对任意请求签名。`extra_headers` 的值必须与真正发送时完全一致。
    pub fn sign(
        &self,
        method: &str,
        url: &str,
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> SignedRequest {
        let (canonical_uri, canonical_query, host) = split_url(url);

        // 全部头名转小写后参与签名
        let mut headers: Vec<(String, String)> = Vec::new();
        for (k, v) in extra_headers {
            headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
        let has = |name: &str, list: &[(String, String)]| {
            list.iter().any(|(k, _)| k == name)
        };
        if !has("host", &headers) {
            headers.push(("host".to_string(), host));
        }
        if !has("content-type", &headers) {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        headers.retain(|(k, _)| k != "x-sdk-date");
        headers.push(("x-sdk-date".to_string(), sdk_date()));

        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let canonical_headers: String = headers
            .iter()
            .map(|(k, v)| format!("{k}:{}\n", v.trim()))
            .collect();
        let signed_headers = headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");

        let payload_hash = sha256_hex(body);
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.to_ascii_uppercase(),
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_headers,
            payload_hash
        );
        let string_to_sign = format!(
            "SDK-HMAC-SHA256\n{}\n{}",
            headers
                .iter()
                .find(|(k, _)| k == "x-sdk-date")
                .map(|(_, v)| v.as_str())
                .unwrap_or(""),
            sha256_hex(canonical_request.as_bytes())
        );
        let signature = hmac_hex(self.sk.as_bytes(), &string_to_sign);

        let authorization = format!(
            "SDK-HMAC-SHA256 Access={}, SignedHeaders={}, Signature={}",
            self.ak, signed_headers, signature
        );

        let mut out: Vec<(String, String)> = headers
            .into_iter()
            .filter(|(k, _)| !matches!(k.as_str(), "host" | "content-type"))
            .collect();
        // 恢复友好大小写，便于调试时阅读；签名本身不关心实际发送的大小写。
        for (k, _v) in out.iter_mut() {
            if k == "x-sdk-date" {
                *k = "X-Sdk-Date".to_string();
            }
        }
        out.push(("Content-Type".to_string(), "application/json".to_string()));
        out.push(("Authorization".to_string(), authorization));

        SignedRequest { headers: out }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_uri_gets_trailing_slash() {
        let (uri, query, host) = split_url("https://example.com/api/v2/chat/completions");
        assert_eq!(uri, "/api/v2/chat/completions/");
        assert_eq!(query, "");
        assert_eq!(host, "example.com");
    }

    #[test]
    fn query_params_are_sorted_and_encoded() {
        let (_, query, _) = split_url("https://example.com/x?b=2&a=hello%20world");
        assert_eq!(query, "a=hello%20world&b=2");
    }

    #[test]
    fn signature_is_deterministic_for_fixed_inputs() {
        let signer = Signer::new("AKID", "SECRET");
        let r1 = signer.sign("POST", "https://example.com/api/v2/chat/completions", b"{}", &[]);
        let auth1 = r1
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(auth1.starts_with("SDK-HMAC-SHA256 Access=AKID, SignedHeaders=content-type;host;x-sdk-date, Signature="));
    }
}
