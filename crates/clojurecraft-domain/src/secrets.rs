//! Keeps credentials out of anything that is persisted or exported.
//!
//! Domain records have no credential fields, and model provenance stores repository coordinates
//! rather than URLs. Credentials can still leak indirectly, for example through an error message
//! or captured process output. The [`SecretRegistry`] holds every secret value the running
//! application knows (inference API keys, access tokens) so that machine-produced text can be
//! redacted and user content containing a secret can be refused before it is written.

use std::borrow::Cow;
use std::sync::RwLock;

use serde::Serialize;
use serde::de::DeserializeOwned;

pub const REDACTED: &str = "[redacted]";

/// Values shorter than this are not treated as secrets; redacting them would mangle ordinary text.
const MIN_SECRET_LEN: usize = 12;

/// Hugging Face user access tokens: `hf_` followed by a long alphanumeric string.
const HF_TOKEN_PREFIX: &str = "hf_";
const HF_TOKEN_MIN_BODY: usize = 30;

#[derive(Default)]
pub struct SecretRegistry {
    secrets: RwLock<Vec<String>>,
}

impl std::fmt::Debug for SecretRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.secrets.read().map_or(0, |s| s.len());
        f.debug_struct("SecretRegistry").field("secrets", &count).finish()
    }
}

impl SecretRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a secret for the rest of the process lifetime.
    pub fn register(&self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.len() < MIN_SECRET_LEN {
            return;
        }
        if let Ok(mut secrets) = self.secrets.write()
            && !secrets.contains(&secret)
        {
            secrets.push(secret);
        }
    }

    /// Whether `text` contains a registered secret verbatim.
    pub fn contains_secret(&self, text: &str) -> bool {
        self.secrets
            .read()
            .is_ok_and(|s| s.iter().any(|secret| text.contains(secret.as_str())))
    }

    /// Replaces registered secrets and access-token-shaped strings in machine-produced text.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        if let Ok(secrets) = self.secrets.read() {
            for secret in secrets.iter() {
                if out.contains(secret.as_str()) {
                    out = Cow::Owned(out.replace(secret.as_str(), REDACTED));
                }
            }
        }
        match redact_hf_tokens(&out) {
            Some(replaced) => Cow::Owned(replaced),
            None => out,
        }
    }

    /// Redacts every string inside a serialisable record.
    pub fn redact_record<T: Serialize + DeserializeOwned>(&self, record: &T) -> serde_json::Result<T> {
        let mut value = serde_json::to_value(record)?;
        self.redact_value(&mut value);
        serde_json::from_value(value)
    }

    fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => {
                if let Cow::Owned(r) = self.redact(s) {
                    *s = r;
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|v| self.redact_value(v)),
            serde_json::Value::Object(map) => map.values_mut().for_each(|v| self.redact_value(v)),
            _ => {}
        }
    }
}

fn redact_hf_tokens(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = text;
    let mut changed = false;
    while let Some(pos) = rest.find(HF_TOKEN_PREFIX) {
        let boundary_ok = rest[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        let body = &rest[pos + HF_TOKEN_PREFIX.len()..];
        let body_len = body.bytes().take_while(u8::is_ascii_alphanumeric).count();
        out.push_str(&rest[..pos]);
        if boundary_ok && body_len >= HF_TOKEN_MIN_BODY {
            out.push_str(REDACTED);
            changed = true;
            rest = &body[body_len..];
        } else {
            out.push_str(HF_TOKEN_PREFIX);
            rest = body;
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "4f9c0d2e8b7a6c5d4e3f2a1b";

    #[test]
    fn registered_secrets_are_redacted_and_detected() {
        let r = SecretRegistry::new();
        r.register(KEY);
        r.register("short");
        let msg = format!("request failed: Authorization: Bearer {KEY}");
        assert!(r.contains_secret(&msg));
        assert_eq!(r.redact(&msg), "request failed: Authorization: Bearer [redacted]");
        assert!(
            !r.contains_secret("short text"),
            "short values are never registered"
        );
        assert!(!format!("{r:?}").contains(KEY));
    }

    #[test]
    fn token_shaped_strings_are_redacted_without_registration() {
        let r = SecretRegistry::new();
        let token = format!("hf_{}", "a1B2".repeat(9));
        let text = format!("401 for token {token}.");
        assert_eq!(r.redact(&text), "401 for token [redacted].");
        // Ordinary identifiers that merely contain the prefix are untouched.
        for benign in [
            "my_hf_value",
            "hf_short",
            "graph_hf_abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            assert_eq!(r.redact(benign), benign);
        }
    }

    #[test]
    fn records_are_redacted_recursively() {
        let r = SecretRegistry::new();
        r.register(KEY);
        let record = serde_json::json!({"a": [format!("x{KEY}y"), {"b": KEY}], "n": 1});
        let redacted = r.redact_record(&record).unwrap();
        assert!(!redacted.to_string().contains(KEY));
        assert_eq!(redacted["n"], 1);
    }
}
