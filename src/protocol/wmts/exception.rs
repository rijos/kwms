//! WMTS 1.0.0 exception reports (OWS Common `ExceptionReport`).
//!
//! WMTS only defines the OWS Common codes plus `TileOutOfRange`, with the
//! HTTP statuses of WMTS 1.0.0 Table 28 — except `OperationNotSupported`,
//! which is 400 here instead of 501: request errors are never 5xx. kwms's more specific codes
//! (`LayerNotDefined`, `InvalidFormat`, …) are reported as
//! `InvalidParameterValue`; the `locator` still names the parameter.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::protocol::error::{OgcCode, OgcError};

/// `(HTTP status, exceptionCode)` for an [`OgcError`] under WMTS.
pub fn wmts_code(code: OgcCode) -> (u16, &'static str) {
    match code {
        OgcCode::MissingParameterValue => (400, "MissingParameterValue"),
        OgcCode::TileOutOfRange => (400, "TileOutOfRange"),
        // WMTS 1.0.0 says 501, but kwms keeps 5xx for server failures only
        // (they page operators); an unsupported operation is a client issue.
        OgcCode::OperationNotSupported => (400, "OperationNotSupported"),
        OgcCode::InvalidParameterValue
        | OgcCode::InvalidCrs
        | OgcCode::LayerNotDefined
        | OgcCode::StyleNotDefined
        | OgcCode::InvalidFormat => (400, "InvalidParameterValue"),
    }
}

/// `(HTTP status, ExceptionReport XML)` for a request error.
pub fn for_error(err: &OgcError) -> (u16, String) {
    let (status, code) = wmts_code(err.code);
    (status, exception_report(code, err.locator_name(), err.message))
}

/// An `ExceptionReport` document (all text XML-escaped).
pub fn exception_report(code: &str, locator: Option<&str>, message: &str) -> String {
    let mut w = Writer::new(Vec::new());
    let _ = w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));
    let mut root = BytesStart::new("ExceptionReport");
    root.push_attribute(("xmlns", "http://www.opengis.net/ows/1.1"));
    root.push_attribute(("version", "1.1.0"));
    let _ = w.write_event(Event::Start(root));
    let mut exc = BytesStart::new("Exception");
    exc.push_attribute(("exceptionCode", code));
    if let Some(l) = locator {
        exc.push_attribute(("locator", l));
    }
    let _ = w.write_event(Event::Start(exc));
    let _ = w.write_event(Event::Start(BytesStart::new("ExceptionText")));
    let _ = w.write_event(Event::Text(BytesText::new(message)));
    let _ = w.write_event(Event::End(BytesEnd::new("ExceptionText")));
    let _ = w.write_event(Event::End(BytesEnd::new("Exception")));
    let _ = w.write_event(Event::End(BytesEnd::new("ExceptionReport")));
    String::from_utf8(w.into_inner()).expect("quick-xml emits UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::error::Param;

    #[test]
    fn maps_codes_statuses_and_escapes() {
        let (status, xml) = for_error(&OgcError::new(OgcCode::LayerNotDefined, Some(Param::Layer), "a '<x>'"));
        assert_eq!(status, 400);
        assert!(
            xml.contains(r#"exceptionCode="InvalidParameterValue" locator="LAYER""#),
            "{xml}"
        );
        assert!(xml.contains("a &apos;&lt;x&gt;&apos;"), "{xml}");
        assert_eq!(wmts_code(OgcCode::TileOutOfRange), (400, "TileOutOfRange"));
        assert_eq!(wmts_code(OgcCode::OperationNotSupported).0, 400, "never 5xx");
    }
}
