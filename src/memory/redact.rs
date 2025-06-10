//! Redaction of secrets before storage or embedding (issue 0029, D21).
//!
//! Applied to candidate text BEFORE embedding (so secrets never reach the
//! provider payload) and before the DB insert (so they never land in
//! `memories.text`). Also applied to turn content inside extraction prompts.

use regex::Regex;
use std::sync::OnceLock;

/// Replacement marker.
pub const REDACTED: &str = "[REDACTED]";

fn patterns() -> &'static Vec<(Regex, &'static str)> {
    static CELL: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    CELL.get_or_init(|| {
        vec![
            // OpenAI-style keys.
            (Regex::new(r"sk-[A-Za-z0-9_-]{8,}").unwrap(), REDACTED),
            // Bearer tokens.
            (
                Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{4,}").unwrap(),
                REDACTED,
            ),
            // PEM private-key blocks.
            (
                Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .unwrap(),
                REDACTED,
            ),
            // password= / secret: style pairs.
            (
                Regex::new(r"(?i)(password|passwd|secret|api[_-]?key)\s*[:=]\s*\S+").unwrap(),
                REDACTED,
            ),
        ]
    })
}

/// Redact secrets in `text`; returns the redacted copy.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for (re, replacement) in patterns() {
        out = re.replace_all(&out, *replacement).into_owned();
    }
    out
}

/// True when redaction changed the text (a secret was present).
pub fn contains_secret(text: &str) -> bool {
    redact(text) != text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_each_pattern() {
        assert_eq!(redact("key sk-abcDEF123456 rest"), "key [REDACTED] rest");
        assert_eq!(redact("auth Bearer token1234!"), "auth [REDACTED]");
        assert_eq!(redact("pw password=hunter2!"), "pw [REDACTED]");
        assert_eq!(redact("nothing secret here"), "nothing secret here");
        assert!(contains_secret("api_key: xyz"));
        assert!(!contains_secret("plain text"));
    }

    #[test]
    fn redacts_pem_block() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redact(pem), REDACTED);
    }
}
