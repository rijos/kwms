//! TMS error documents. TMS 1.0.0 defines no exception codes, so kwms
//! reports the OGC code name and uses HTTP statuses: unknown layer or a
//! tile outside the pyramid → 404, anything else → 400 (request errors are
//! never 5xx).

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::protocol::error::{OgcCode, OgcError};

/// `(HTTP status, error XML)` for a request error.
pub fn for_error(err: &OgcError) -> (u16, String) {
    let status = match err.code {
        OgcCode::LayerNotDefined | OgcCode::TileOutOfRange => 404,
        _ => 400,
    };
    (status, error_document(err.code.as_str(), err.message))
}

/// A `<TileMapServerError>` document (text XML-escaped).
pub fn error_document(code: &str, message: &str) -> String {
    let mut w = Writer::new(Vec::new());
    let _ = w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)));
    let _ = w.write_event(Event::Start(BytesStart::new("TileMapServerError")));
    for (tag, text) in [("Code", code), ("Message", message)] {
        let _ = w.write_event(Event::Start(BytesStart::new(tag)));
        let _ = w.write_event(Event::Text(BytesText::new(text)));
        let _ = w.write_event(Event::End(BytesEnd::new(tag)));
    }
    let _ = w.write_event(Event::End(BytesEnd::new("TileMapServerError")));
    String::from_utf8(w.into_inner()).expect("quick-xml emits UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::error::Param;

    #[test]
    fn statuses_and_escaping() {
        // Messages are static literals; escaping still holds for any we write.
        let (s, xml) = for_error(&OgcError::new(OgcCode::LayerNotDefined, None, "a <a> b"));
        assert_eq!(s, 404);
        assert!(
            xml.contains("<Code>LayerNotDefined</Code>") && xml.contains("&lt;a&gt;"),
            "{xml}"
        );
        assert_eq!(for_error(&OgcError::invalid(Param::X, "bad")).0, 400);
    }
}
