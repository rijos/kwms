//! WMS service-exception XML.
//!
//! Both 1.1.1 and 1.3.0 share the same `<ServiceExceptionReport>`/
//! `<ServiceException>` shape; they differ only in:
//!   - the `version` attribute on the root element;
//!   - the default XML namespace (1.3.0 binds the OGC namespace).
//!
//! kwms emits no DOCTYPE — the WMS 1.1.1 spec's external DTD reference
//! is universally ignored by modern clients and most servers (PDOK
//! included) have moved off it.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use super::request::WmsVersion;

/// Build a `<ServiceExceptionReport>` document.
///
/// `code` populates the `code=` attribute (e.g. `"InvalidParameterValue"`,
/// `"MissingParameterValue"`, `"InvalidCRS"`). `message` becomes the text
/// content of `<ServiceException>`. Both are XML-escaped.
pub fn service_exception(code: &str, message: &str, version: WmsVersion) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut w = Writer::new(&mut buf);

    // XML declaration. WMS clients are not strict about it but it's good
    // hygiene and matches what PDOK / GeoServer emit.
    let _ = w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));

    let mut root = BytesStart::new("ServiceExceptionReport");
    root.push_attribute(("version", version.as_str()));
    // 1.3.0 binds the OGC namespace; 1.1.1 omits xmlns (the DTD identifies
    // the document by root element name alone). We deliberately do not
    // emit the 1.1.1 DOCTYPE — see module docs.
    if matches!(version, WmsVersion::V1_3_0) {
        root.push_attribute(("xmlns", "http://www.opengis.net/ogc"));
    }
    let _ = w.write_event(Event::Start(root));

    let mut exc = BytesStart::new("ServiceException");
    exc.push_attribute(("code", code));
    let _ = w.write_event(Event::Start(exc));
    let _ = w.write_event(Event::Text(BytesText::new(message)));
    let _ = w.write_event(Event::End(BytesEnd::new("ServiceException")));

    let _ = w.write_event(Event::End(BytesEnd::new("ServiceExceptionReport")));

    String::from_utf8(buf).expect("quick-xml emits valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_share_root_name_and_carry_code_and_message() {
        let xml111 = service_exception("InvalidCRS", "bad CRS: EPSG:99999", WmsVersion::V1_1_1);
        let xml130 = service_exception("InvalidCRS", "bad CRS: EPSG:99999", WmsVersion::V1_3_0);

        // Both versions use the same root element name (the spec didn't
        // rename it, only added the namespace).
        assert!(xml111.contains("<ServiceExceptionReport"));
        assert!(xml130.contains("<ServiceExceptionReport"));

        // Version attribute is per-version.
        assert!(
            xml111.contains("version=\"1.1.1\""),
            "1.1.1 XML missing version: {xml111}"
        );
        assert!(
            xml130.contains("version=\"1.3.0\""),
            "1.3.0 XML missing version: {xml130}"
        );

        // Only 1.3.0 binds the OGC namespace.
        assert!(!xml111.contains("xmlns="), "1.1.1 should not declare xmlns");
        assert!(
            xml130.contains("xmlns=\"http://www.opengis.net/ogc\""),
            "1.3.0 missing OGC xmlns"
        );

        // Both carry the code attribute and the message text.
        for xml in [&xml111, &xml130] {
            assert!(xml.contains("code=\"InvalidCRS\""), "missing code in {xml}");
            assert!(xml.contains("bad CRS: EPSG:99999"), "missing message in {xml}");
        }
    }

    #[test]
    fn message_special_chars_are_escaped() {
        let xml = service_exception("X", "a < b & c > d", WmsVersion::V1_3_0);
        // quick-xml's `BytesText::new` escapes special chars on write.
        assert!(xml.contains("a &lt; b &amp; c &gt; d"), "got: {xml}");
        assert!(!xml.contains("a < b & c > d"));
    }
}
