//! Remove credentials from text before it is logged or returned.
//!
//! Object-store errors carry the URL they failed on, and with a SAS token
//! that URL *is* the credential (`…?sv=…&sig=…`). GDAL's `/vsiaz/` warnings
//! print it, and so does opendal's error context. Everything that may hold
//! such a URL — GDAL messages, cache tier errors — goes through [`secrets`]
//! before it reaches a log line or an error chain.

use std::borrow::Cow;

/// Replacement for a removed value.
pub const REDACTED: &str = "<redacted>";

/// Query parameters and `KEY=value` pairs whose value is a credential,
/// wherever they appear (lower-case; matched case-insensitively).
const SECRET_KEYS: &[&str] = &[
    "sig",
    "signature",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-goog-signature",
    "x-goog-credential",
    "accountkey",
    "sharedaccesssignature",
    "azure_storage_sas_token",
    "azure_storage_access_key",
    "azure_storage_account_key",
    "aws_secret_access_key",
    "aws_session_token",
    "access_token",
    "token",
];

/// `text` with every URL query string and every `secret_key=value` pair
/// replaced by [`REDACTED`]. Borrowed when there was nothing to remove.
pub fn secrets(text: &str) -> Cow<'_, str> {
    if !needs_work(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(q) = rest.find('?') {
        let (before, after) = rest.split_at(q);
        out.push_str(before);
        // A '?' inside a URL (after "scheme://…") starts its query string;
        // anywhere else it is ordinary punctuation.
        if in_url(before) {
            out.push('?');
            out.push_str(REDACTED);
            let end = after.find(url_end).unwrap_or(after.len());
            rest = &after[end..];
        } else {
            out.push('?');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    Cow::Owned(redact_pairs(&out))
}

fn needs_work(text: &str) -> bool {
    text.contains('?') || {
        let lower = text.to_ascii_lowercase();
        SECRET_KEYS.iter().any(|k| lower.contains(&format!("{k}=")))
    }
}

/// Whether the text right before a '?' is inside a URL: the current
/// whitespace-delimited word contains "://".
fn in_url(before: &str) -> bool {
    let word_start = before.rfind(url_end).map_or(0, |i| i.saturating_add(1));
    before[word_start..].contains("://")
}

fn url_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | ')' | ']' | '}')
}

/// Replace the value of every `secret_key=value` (value up to `&`, `;`,
/// `,`, whitespace or a quote).
fn redact_pairs(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let hit = SECRET_KEYS.iter().find_map(|k| {
            let starts = lower[i..].starts_with(k) && lower.as_bytes()[i..].get(k.len()) == Some(&b'=');
            // Whole key only: `xsig=` isn't `sig=`.
            let boundary = i == 0
                || !lower.as_bytes()[i.saturating_sub(1)].is_ascii_alphanumeric()
                    && lower.as_bytes()[i.saturating_sub(1)] != b'_'
                    && lower.as_bytes()[i.saturating_sub(1)] != b'-';
            (starts && boundary).then_some(k.len())
        });
        match hit {
            Some(len) => {
                let value_start = i.saturating_add(len).saturating_add(1);
                out.push_str(&text[i..value_start]);
                out.push_str(REDACTED);
                let value_len = text[value_start..]
                    .find(|c: char| c.is_whitespace() || matches!(c, '&' | ';' | ',' | '"' | '\'' | '`'))
                    .unwrap_or(text.len().saturating_sub(value_start));
                i = value_start.saturating_add(value_len);
            }
            None => {
                let ch = text[i..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&text[i..i.saturating_add(ch)]);
                i = i.saturating_add(ch);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_query_strings_are_removed() {
        let msg = "HTTP response code on https://acct.blob.core.windows.net/c/x.tif?sv=2025-07-05&se=2026&sr=c&sp=rl&sig=AbC%3D: 403";
        assert_eq!(
            secrets(msg),
            "HTTP response code on https://acct.blob.core.windows.net/c/x.tif?<redacted> 403"
        );
        let ctx = r#"context: { uri: http://127.0.0.1:1/a/b?sv=1&sig=XYZ, called: read }"#;
        assert_eq!(
            secrets(ctx),
            "context: { uri: http://127.0.0.1:1/a/b?<redacted> called: read }"
        );
    }

    #[test]
    fn secret_pairs_are_removed_anywhere() {
        assert_eq!(
            secrets("AccountName=a;AccountKey=c2VjcmV0;EndpointSuffix=x"),
            "AccountName=a;AccountKey=<redacted>;EndpointSuffix=x"
        );
        assert_eq!(
            secrets("set AZURE_STORAGE_SAS_TOKEN=sv=1&sig=2 failed"),
            "set AZURE_STORAGE_SAS_TOKEN=<redacted>&sig=<redacted> failed"
        );
        assert_eq!(secrets("sig=abc"), "sig=<redacted>");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for text in [
            "Is this a question? Yes.",
            "driver GTI does not support open option RESAMPLING",
            "design=1 xsig=2",
            "",
        ] {
            assert!(
                matches!(secrets(text), Cow::Borrowed(_)) || secrets(text) == text,
                "{text}"
            );
            assert_eq!(secrets(text), text);
        }
    }
}
