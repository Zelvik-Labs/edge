pub mod captcha;
pub mod click;
pub mod jscookie;
pub mod slider;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::Engine;
use rand::RngExt;
use serde_json::Value;

/// Type of challenge served to the user-agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChallengeType {
    Slider,
    Click,
    Captcha,
    Pow,
    JsCookie,
}

impl ChallengeType {
    pub fn from_method(method: &str) -> Self {
        match method {
            "slider" => Self::Slider,
            "click" => Self::Click,
            "captcha" => Self::Captcha,
            "pow" => Self::Pow,
            "jscookie" => Self::JsCookie,
            _ => Self::Slider,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Slider => "slider",
            Self::Click => "click",
            Self::Captcha => "captcha",
            Self::Pow => "pow",
            Self::JsCookie => "jscookie",
        }
    }
}

// ── Token encode / decode (AES-256-GCM) ─────────────────────────

/// Encode a payload JSON into an encrypted, URL-safe token.
///
/// Format: `base64_urlsafe_nopad( 12-byte-AES-nonce ‖ AES-256-GCM-ciphertext )`
///
/// `key` is the node's derived challenge key (`ApiConfig::challenge_key`,
/// 04 §2.4 / 05 §4.6, D7) used directly as the AES-256 key. The raw credential
/// secret never reaches this function, and there is no second derivation here.
pub(crate) fn encode_challenge_token(payload: &Value, key: &[u8; 32]) -> String {
    let cipher = Aes256Gcm::new_from_slice(key).expect("valid AES-256 key");

    let mut aes_nonce = [0u8; 12];
    rand::rng().fill(&mut aes_nonce);
    let nonce = Nonce::from(aes_nonce);

    let plaintext = serde_json::to_string(payload).expect("payload serialization");
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("AES-GCM encryption");

    let mut combined = Vec::with_capacity(12 + ciphertext.len());
    combined.extend_from_slice(&aes_nonce);
    combined.extend_from_slice(&ciphertext);

    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&combined)
}

/// Decrypt and decode a challenge token back to its JSON payload.
///
/// Returns `None` when the token is malformed, tampered, or decryption fails.
pub(crate) fn decode_challenge_token(token: &str, key: &[u8; 32]) -> Option<Value> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("valid AES-256 key");

    let combined = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token)
        .ok()?;
    if combined.len() < 12 {
        return None;
    }

    let (aes_nonce, ciphertext) = combined.split_at(12);
    let nonce = Nonce::from(<[u8; 12]>::try_from(aes_nonce).ok()?);

    let plaintext = cipher.decrypt(&nonce, ciphertext).ok()?;
    serde_json::from_slice(&plaintext).ok()
}

/// Check whether the payload's `e` (expiry unix-seconds) field has passed.
pub(crate) fn is_token_expired(payload: &Value) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    payload
        .get("e")
        .and_then(|v| v.as_u64())
        .is_none_or(|exp| now > exp)
}

/// Generate an 8-character random alphanumeric suffix for JS obfuscation.
pub(crate) fn random_suffix() -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| {
            let idx = rng.random_range(0..CHARS.len());
            CHARS[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn challenge_token_is_keyed_by_the_supplied_challenge_key() {
        let key = [0x11u8; 32];
        let payload = json!({"t": "slider", "p": {"tx": 1.0}, "e": 4_102_444_800u64});

        let token = encode_challenge_token(&payload, &key);
        assert_eq!(decode_challenge_token(&token, &key), Some(payload));

        // A different challenge key cannot decode it: the token is keyed by the
        // derived value itself, with no second derivation and no fallback.
        let other = [0x12u8; 32];
        assert_eq!(decode_challenge_token(&token, &other), None);
    }
}
