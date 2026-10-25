//! DPoP（RFC 9449）证明 JWT：ES256 + P-256。
//!
//! 华为云 STS 的 `oauth2/tokens` 端点要求请求头携带 DPoP 证明，
//! 并且会把 refresh_token 绑定到首次换取时的 DPoP 公钥：
//! 后续刷新必须用同一对密钥，否则返回 `InvalidDPoPHeader`。
//! 因此私钥必须与 refresh_token 一起持久化。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdsa::{signature::Signer, Signature, SigningKey, VerifyingKey};
use rand::RngCore;
use serde_json::{json, Value};

/// 可持久化的 DPoP 私钥（JWK 形式）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DpopKey {
    pub kty: String,
    pub crv: String,
    pub x: String,
    pub y: String,
    pub d: String,
}

fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

impl DpopKey {
    /// 生成新的 P-256 密钥对。
    pub fn generate() -> Self {
        // p256 0.14 的 random 需要 rand_core 0.10 的 CryptoRng；
        // 这里直接取 32 字节随机数构造签名密钥，避开版本适配问题。
        let signing = loop {
            let mut seed = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
            if let Ok(k) = SigningKey::from_slice(&seed) {
                break k;
            }
        };
        let verifying = VerifyingKey::from(&signing);
        let encoded = verifying.to_sec1_point(false);
        let x = encoded.x().map(|b| b.to_vec()).unwrap_or_default();
        let y = encoded.y().map(|b| b.to_vec()).unwrap_or_default();
        let d = signing.to_bytes().to_vec();
        Self {
            kty: "EC".to_string(),
            crv: "P-256".to_string(),
            x: b64(&x),
            y: b64(&y),
            d: b64(&d),
        }
    }

    fn signing_key(&self) -> Result<SigningKey, String> {
        let d = URL_SAFE_NO_PAD
            .decode(&self.d)
            .map_err(|e| format!("DPoP 私钥解析失败: {e}"))?;
        SigningKey::from_slice(&d).map_err(|e| format!("DPoP 私钥无效: {e}"))
    }

    /// 公钥 JWK（放进 JWT header）。
    fn public_jwk(&self) -> Value {
        json!({
            "kty": self.kty,
            "crv": self.crv,
            "x": self.x,
            "y": self.y,
        })
    }

    /// 生成发往指定 token 端点的 DPoP 证明。
    pub fn proof(&self, htu: &str) -> Result<String, String> {
        let signing = self.signing_key()?;
        let header = json!({
            "alg": "ES256",
            "typ": "dpop+jwt",
            "jwk": self.public_jwk(),
        });
        let payload = json!({
            "htm": "POST",
            "htu": htu,
            "iat": chrono::Utc::now().timestamp(),
            "jti": random_hex(16),
        });

        let input = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(payload.to_string().as_bytes())
        );

        let signature: Signature = signing.sign(input.as_bytes());
        // p256 默认输出低 S 归一化签名，与 jose 一致
        Ok(format!("{input}.{}", b64(&signature.to_bytes())))
    }
}

/// PKCE：生成 (code_verifier, code_challenge)。
pub fn pkce() -> (String, String) {
    let verifier = random_hex(32);
    let challenge = b64(&sha256(verifier.as_bytes()));
    (verifier, challenge)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_valid_jwk() {
        let key = DpopKey::generate();
        assert_eq!(key.kty, "EC");
        assert_eq!(key.crv, "P-256");
        // 32 字节坐标 base64url 无填充 = 43 字符
        assert_eq!(key.x.len(), 43);
        assert_eq!(key.y.len(), 43);
        assert_eq!(key.d.len(), 43);
    }

    #[test]
    fn produces_three_part_jwt() {
        let key = DpopKey::generate();
        let proof = key.proof("https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens").unwrap();
        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(parts.len(), 3);

        // header 可解出 alg/typ/jwk
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "dpop+jwt");
        assert_eq!(header["jwk"]["crv"], "P-256");

        // payload 含 htm/htu/iat/jti
        let payload: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["htm"], "POST");
        assert!(payload["htu"].as_str().unwrap().contains("/v1/oauth2/tokens"));
        assert!(payload["iat"].is_i64());
        assert!(payload["jti"].is_string());
    }

    #[test]
    fn roundtrips_through_json() {
        let key = DpopKey::generate();
        let text = serde_json::to_string(&key).unwrap();
        let back: DpopKey = serde_json::from_str(&text).unwrap();
        // 同一私钥必须能复现可用签名
        assert!(back.proof("https://example.com/token").is_ok());
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (verifier, challenge) = pkce();
        assert_eq!(verifier.len(), 64); // 32 字节 hex
        assert_eq!(challenge, b64(&sha256(verifier.as_bytes())));
    }
}
