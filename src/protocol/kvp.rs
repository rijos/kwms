//! `Kvp` — the one parser for OGC key-value-pair query strings (WMS, WMTS).
//!
//! Rules, in order:
//! 1. The whole query is at most [`MAX_QUERY_BYTES`] bytes.
//! 2. Pairs are separated by `&`; empty pairs (`a=1&&b=2`) are skipped.
//! 3. A pair without `=` is a key with an empty value (`TRANSPARENT`).
//! 4. Keys and values are form-decoded exactly once: `+` → space, then
//!    `%XX` → byte. A `%` not followed by two hex digits, or a result that
//!    is not UTF-8, is rejected.
//! 5. Keys are matched case-insensitively (OGC KVP); an empty key, a key
//!    over [`MAX_KEY_BYTES`], or a value over [`MAX_VALUE_BYTES`] is rejected.
//! 6. The same key twice (in any case) is rejected: we won't guess which
//!    one the client meant.
//! 7. At most [`MAX_PARAMS`] pairs.
//! 8. Unknown keys are kept but never required — OGC servers must ignore
//!    vendor parameters they don't understand.
//!
//! Every rejection is an [`OgcError`] with `InvalidParameterValue` and a
//! fixed message. Its locator is one of our [`Param`]s, or none when the
//! offending key is not one we define — a client-chosen key is never echoed.

use std::collections::{HashMap, HashSet};

use percent_encoding::percent_decode_str;

use super::error::{OgcCode, OgcError, Param};

pub const MAX_QUERY_BYTES: usize = 8 * 1024;
pub const MAX_PARAMS: usize = 64;
pub const MAX_KEY_BYTES: usize = 64;
pub const MAX_VALUE_BYTES: usize = 4 * 1024;

/// A parsed, decoded query string: the values of the parameters we know.
/// (Unknown vendor parameters are checked for the rules above, then dropped.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kvp {
    params: HashMap<Param, String>,
}

impl Kvp {
    pub fn parse(query: &str) -> Result<Self, OgcError> {
        let query = query.strip_prefix('?').unwrap_or(query);
        if query.len() > MAX_QUERY_BYTES {
            return Err(malformed(None, "query string is too long"));
        }
        let mut params = HashMap::new();
        let mut seen = HashSet::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            if seen.len() == MAX_PARAMS {
                return Err(malformed(None, "too many query parameters"));
            }
            let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode(raw_key, None)?.to_ascii_lowercase();
            if key.is_empty() {
                return Err(malformed(None, "query parameter with an empty name"));
            }
            if key.len() > MAX_KEY_BYTES {
                return Err(malformed(None, "query parameter name is too long"));
            }
            let param = Param::from_key(&key);
            let value = decode(raw_value, param)?;
            if value.len() > MAX_VALUE_BYTES {
                return Err(malformed(param, "query parameter value is too long"));
            }
            if !seen.insert(key) {
                return Err(malformed(param, "query parameter given more than once"));
            }
            if let Some(p) = param {
                params.insert(p, value);
            }
        }
        Ok(Self { params })
    }

    /// The value of `param`, if present.
    pub fn get(&self, param: Param) -> Option<&str> {
        self.params.get(&param).map(String::as_str)
    }

    /// The value of `param`, or `MissingParameterValue`.
    pub fn require(&self, param: Param) -> Result<&str, OgcError> {
        self.get(param).ok_or(OgcError::missing(param))
    }

    pub fn contains(&self, param: Param) -> bool {
        self.params.contains_key(&param)
    }
}

/// `InvalidParameterValue` with a fixed message.
fn malformed(locator: Option<Param>, message: &'static str) -> OgcError {
    OgcError::new(OgcCode::InvalidParameterValue, locator, message)
}

/// Form-decode one key or value: `+` → space, `%XX` → byte, strict UTF-8.
fn decode(raw: &str, locator: Option<Param>) -> Result<String, OgcError> {
    let bytes = raw.as_bytes();
    for (i, _) in raw.match_indices('%') {
        // The two bytes after the '%' (no index arithmetic).
        let hex = bytes.get(i..).and_then(|rest| rest.get(1..3));
        if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
            return Err(malformed(locator, "malformed percent-escape in query string"));
        }
    }
    let plus_as_space = raw.replace('+', " ");
    percent_decode_str(&plus_as_space)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| malformed(locator, "query string is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_once_and_matches_keys_case_insensitively() {
        let kvp = Kvp::parse("?SERVICE=WMS&crs=EPSG%3A3857&Layers=a%2Cb&Format=image%2Fpng+x&TRANSPARENT")
            .unwrap();
        assert_eq!(kvp.get(Param::Service), Some("WMS"));
        assert_eq!(kvp.get(Param::Crs), Some("EPSG:3857"));
        assert_eq!(kvp.get(Param::Layers), Some("a,b"));
        assert_eq!(kvp.get(Param::Format), Some("image/png x"));
        assert_eq!(kvp.get(Param::Transparent), Some(""));
        // Decoded exactly once: %2541 is "%41", not "A".
        assert_eq!(Kvp::parse("bbox=%2541").unwrap().get(Param::Bbox), Some("%41"));
        // A literal '+' must be sent as %2B.
        assert_eq!(Kvp::parse("bbox=a%2Bb").unwrap().get(Param::Bbox), Some("a+b"));
        // Unknown vendor parameters are accepted and ignored.
        assert_eq!(Kvp::parse("vendor=caf%C3%A9").unwrap(), Kvp::default());
    }

    #[test]
    fn empty_query_and_empty_pairs() {
        assert_eq!(Kvp::parse("").unwrap(), Kvp::default());
        let kvp = Kvp::parse("width=1&&height=2&").unwrap();
        assert_eq!((kvp.get(Param::Width), kvp.get(Param::Height)), (Some("1"), Some("2")));
    }

    #[test]
    fn rejects_duplicates_in_any_case() {
        let e = Kvp::parse("LAYERS=a&layers=b").unwrap_err();
        assert_eq!((e.code, e.locator), (OgcCode::InvalidParameterValue, Some(Param::Layers)));
        // Duplicates that only differ by encoding are duplicates too.
        assert!(Kvp::parse("crs=a&%63rs=b").is_err());
        // Unknown keys too — and their name is not echoed as a locator.
        assert_eq!(Kvp::parse("evil=1&EVIL=2").unwrap_err().locator, None);
    }

    #[test]
    fn rejects_malformed_escapes_and_bad_utf8() {
        for q in ["x=%", "x=%4", "x=%zz", "x=100%", "x=%C3%28", "%ZZ=1"] {
            let e = Kvp::parse(q).unwrap_err();
            assert_eq!(e.code, OgcCode::InvalidParameterValue, "{q}");
        }
        assert_eq!(Kvp::parse("bbox=%zz").unwrap_err().locator, Some(Param::Bbox));
    }

    #[test]
    fn rejects_empty_names_and_oversized_input() {
        assert!(Kvp::parse("=1").is_err());
        assert!(Kvp::parse(&format!("{}=1", "k".repeat(MAX_KEY_BYTES + 1))).is_err());
        assert!(Kvp::parse(&format!("v={}", "x".repeat(MAX_VALUE_BYTES + 1))).is_err());
        assert!(Kvp::parse(&"a".repeat(MAX_QUERY_BYTES + 1)).is_err());
        let many: Vec<String> = (0..=MAX_PARAMS).map(|i| format!("p{i}=1")).collect();
        assert!(Kvp::parse(&many.join("&")).is_err());
        let ok: Vec<String> = (0..MAX_PARAMS).map(|i| format!("p{i}=1")).collect();
        assert!(Kvp::parse(&ok.join("&")).is_ok());
    }

    #[test]
    fn require_reports_missing_parameter() {
        let e = Kvp::parse("a=1").unwrap().require(Param::Bbox).unwrap_err();
        assert_eq!((e.code, e.locator), (OgcCode::MissingParameterValue, Some(Param::Bbox)));
    }
}
